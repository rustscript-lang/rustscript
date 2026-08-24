use super::*;
use crate::builtins::BuiltinFunction;
use crate::bytecode::TypeMap;
use crate::compiler::TypeSchema;
use crate::resource::ResourceResult;
use crate::vm::execution_scope::ScopeState;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

fn native_cache_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// A host async bridge that accepts submitted futures and never completes
/// them: used to register real execution-scope operations in tests that
/// exercise the wait/complete contract without a fabricated pending id.
struct NoopPendingBridge;

impl HostAsyncBridge for NoopPendingBridge {
    fn submit_op(&mut self, op_id: HostOpId, future: HostFuture) -> VmResult<()> {
        let _ = (op_id, future);
        Ok(())
    }

    fn poll_op(
        &mut self,
        _op_id: HostOpId,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<VmResult<CallReturn>> {
        std::task::Poll::Pending
    }
}

struct NeverReadyOperation;

#[derive(Clone, Debug, PartialEq, Eq)]
enum VmDropOrderEvent {
    Operation(crate::vm::operation::OperationCancelReason),
    Resource(&'static str, ResourceCloseReason),
}

struct VmDropOrderResource {
    name: &'static str,
    pending: bool,
    events: Arc<Mutex<Vec<VmDropOrderEvent>>>,
}

impl HostResource for VmDropOrderResource {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(ResourceTypeKey::new("test.vm-drop-order").unwrap())
    }

    fn begin_close(&mut self, reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.events
            .lock()
            .unwrap()
            .push(VmDropOrderEvent::Resource(self.name, reason));
        Ok(if self.pending {
            CloseProgress::Pending
        } else {
            CloseProgress::Ready
        })
    }

    fn poll_close(&mut self, _cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        Poll::Pending
    }
}

struct VmDropOrderOperation {
    events: Arc<Mutex<Vec<VmDropOrderEvent>>>,
}

impl crate::vm::operation::HostOperation for VmDropOrderOperation {
    fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<crate::vm::operation::OperationResult<()>> {
        Poll::Pending
    }

    fn cancel(
        &mut self,
        reason: crate::vm::operation::OperationCancelReason,
    ) -> crate::vm::operation::OperationResult<()> {
        self.events
            .lock()
            .unwrap()
            .push(VmDropOrderEvent::Operation(reason));
        Ok(())
    }
}

#[test]
fn vm_drop_owns_guest_cleanup_and_orders_operation_before_child_first_resources() {
    let resource_key = ResourceTypeKey::new("test.vm-drop-order").unwrap();
    let program = Program::new(Vec::new(), Vec::new())
        .with_local_count(1)
        .with_type_map(TypeMap {
            local_schemas: vec![Some(TypeSchema::Resource(resource_key))],
            ..TypeMap::default()
        });
    let mut vm = Vm::try_new(program).unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let (grandparent, parent, child) = {
        let mut host = vm.host_context();
        let grandparent = host
            .push_resource(VmDropOrderResource {
                name: "grandparent",
                pending: false,
                events: Arc::clone(&events),
            })
            .unwrap();
        let parent = host
            .push_child_resource(
                VmDropOrderResource {
                    name: "parent",
                    pending: false,
                    events: Arc::clone(&events),
                },
                &grandparent,
            )
            .unwrap();
        let child = host
            .push_child_resource(
                VmDropOrderResource {
                    name: "child",
                    pending: true,
                    events: Arc::clone(&events),
                },
                &parent,
            )
            .unwrap();
        for handle in [grandparent.handle(), parent.handle(), child.handle()] {
            host.mark_resource_guest_owned(handle).unwrap();
        }
        host.start_operation(
            crate::vm::operation::OperationSpec::new(VmDropOrderOperation {
                events: Arc::clone(&events),
            })
            .with_resource(child.handle()),
        )
        .unwrap();
        (grandparent, parent, child)
    };
    vm.instance.locals[0] = Value::Int(i64::try_from(child.handle().raw()).unwrap());
    let _owners = (grandparent, parent, child);

    drop(vm);

    assert_eq!(
        events.lock().unwrap().as_slice(),
        &[
            VmDropOrderEvent::Operation(crate::vm::operation::OperationCancelReason::VmDrop,),
            VmDropOrderEvent::Resource("child", ResourceCloseReason::VmDrop),
            VmDropOrderEvent::Resource("parent", ResourceCloseReason::VmDrop),
            VmDropOrderEvent::Resource("grandparent", ResourceCloseReason::VmDrop),
        ]
    );
}

impl crate::vm::operation::HostOperation for NeverReadyOperation {
    fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<crate::vm::operation::OperationResult<()>> {
        Poll::Pending
    }

    fn cancel(
        &mut self,
        _reason: crate::vm::operation::OperationCancelReason,
    ) -> crate::vm::operation::OperationResult<()> {
        Ok(())
    }
}

#[test]
fn waiting_admission_rejects_every_occupied_terminal_operation() {
    let mut vm = Vm::try_new(Program::new(Vec::new(), vec![OpCode::Ret as u8])).unwrap();
    let completed = vm
        .host_context()
        .start_operation(crate::vm::operation::OperationSpec::new(
            NeverReadyOperation,
        ))
        .unwrap();
    vm.host
        .execution_scope_complete_operation(completed)
        .unwrap();

    let cancelled = vm
        .host_context()
        .start_operation(crate::vm::operation::OperationSpec::new(
            NeverReadyOperation,
        ))
        .unwrap();
    vm.host
        .execution_scope_cancel_operation(
            cancelled,
            crate::vm::operation::OperationCancelReason::Requested,
        )
        .unwrap();

    let failed = vm
        .host_context()
        .start_operation(crate::vm::operation::OperationSpec::new(
            NeverReadyOperation,
        ))
        .unwrap();
    vm.host
        .execution_scope_fail_operation(
            failed,
            crate::vm::operation::OperationError::new(
                crate::vm::operation::OperationErrorCode::OperationDriverFailed,
                "test",
                "external failure",
            ),
        )
        .unwrap();

    for id in [completed, cancelled, failed] {
        let error = vm
            .set_waiting_host_op(id.raw())
            .expect_err("terminal operation must not be admitted as Waiting");
        assert!(matches!(
            error,
            VmError::Operation(ref operation)
                if operation.code()
                    == crate::vm::operation::OperationErrorCode::OperationNotPending
        ));
        assert!(vm.waiting_host_op_id().is_none());
    }
    assert_eq!(vm.host.execution_scope_operation_count(), 3);
}

#[test]
fn vm_try_new_preserves_operation_registry_tag_exhaustion() {
    static COUNTER: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(crate::vm::operation::id::MAX_REGISTRY_TAG + 1);
    let _source = crate::vm::operation::id::test_seam::ScopedRegistryTagSource::install(&COUNTER);

    let error = match Vm::try_new(Program::new(Vec::new(), vec![OpCode::Ret as u8])) {
        Ok(_) => panic!("operation registry tag exhaustion must fail VM construction"),
        Err(error) => error,
    };
    let VmError::Operation(error) = error else {
        panic!("expected a structured modern operation error");
    };
    assert_eq!(
        error.code(),
        crate::vm::operation::OperationErrorCode::OperationRegistryTagExhausted
    );
    assert_eq!(
        error.limit(),
        Some(crate::vm::operation::id::MAX_REGISTRY_TAG)
    );
    assert_eq!(
        error.value(),
        Some(crate::vm::operation::id::MAX_REGISTRY_TAG + 1)
    );
}

#[test]
fn operation_tag_exhaustion_during_recycle_poisoned_vm_keeps_old_scope_and_drops_cleanly() {
    let mut vm = Vm::try_new(Program::new(Vec::new(), vec![OpCode::Ret as u8]))
        .expect("test VM construction must succeed before the seam is installed");
    let old_arena_id = vm.host.execution_scope().resources().arena_id();
    vm.begin_reset_for_reuse(ResourceCloseReason::VmReset, None)
        .expect("reset should begin");

    let error = {
        static COUNTER: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(crate::vm::operation::id::MAX_REGISTRY_TAG + 1);
        let _source =
            crate::vm::operation::id::test_seam::ScopedRegistryTagSource::install(&COUNTER);
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        match vm.poll_reset_for_reuse(&mut cx, std::time::Instant::now()) {
            Poll::Ready(Ok(())) => panic!("operation exhaustion must poison recycle"),
            Poll::Pending => panic!("empty scope close should reach recycle immediately"),
            Poll::Ready(Err(error)) => error,
        }
    };

    let VmError::Reset(VmResetError::ScopeRecycle(ExecutionScopeError::Operation(operation))) =
        error
    else {
        panic!("expected typed operation scope-recycle failure");
    };
    assert_eq!(
        operation.code(),
        crate::vm::operation::OperationErrorCode::OperationRegistryTagExhausted
    );
    assert_eq!(vm.reset_state(), VmResetState::Poisoned);
    assert!(
        !vm.is_reusable(),
        "a failed recycle must never re-enter the pool"
    );
    assert_eq!(
        vm.host.execution_scope().resources().arena_id(),
        old_arena_id,
        "failed replacement must not swap out the old scope"
    );
    assert_eq!(
        vm.host.execution_scope_state(),
        ScopeState::Quiescent,
        "the preserved old scope remains the quiescent scope that failed replacement"
    );
    assert!(matches!(
        vm.run(),
        Err(VmError::Reset(VmResetError::NotReusable {
            state: VmResetState::Poisoned,
            ..
        }))
    ));
    assert!(matches!(
        vm.begin_reset_for_reuse(ResourceCloseReason::VmReset, None),
        Err(VmError::Reset(VmResetError::AlreadyPoisoned { .. }))
    ));
    drop(vm);

    let fresh = Vm::try_new(Program::new(Vec::new(), vec![OpCode::Ret as u8]))
        .expect("a fresh independent VM succeeds after the seam guard is removed");
    assert!(fresh.is_reusable());
}

#[test]
fn root_ret_completes_explicit_halt_frame() {
    let mut vm = Vm::try_new(Program::new(Vec::new(), vec![OpCode::Ret as u8]))
        .expect("test VM construction must not fail");
    assert_eq!(vm.instance.execution_frames.len(), 1);
    assert_eq!(
        vm.instance.execution_frames[0].continuation,
        FrameContinuation::Halt
    );

    assert_eq!(vm.run().expect("root ret should run"), VmStatus::Halted);
    assert!(vm.instance.execution_frames.is_empty());
    assert!(vm.stack().is_empty());

    vm.reset_for_reuse();
    assert_eq!(vm.instance.execution_frames.len(), 1);
    assert_eq!(vm.stack(), &[]);
}

#[test]
fn async_host_future_is_submitted_to_the_host_bridge() {
    use std::sync::{Arc, Mutex};

    struct RecordingBridge {
        submitted: Arc<Mutex<Vec<HostOpId>>>,
        future: Arc<Mutex<Option<HostFuture>>>,
    }

    impl HostAsyncBridge for RecordingBridge {
        fn submit_op(&mut self, op_id: HostOpId, future: HostFuture) -> VmResult<()> {
            self.submitted.lock().expect("submitted lock").push(op_id);
            *self.future.lock().expect("future lock") = Some(future);
            Ok(())
        }

        fn poll_op(
            &mut self,
            _op_id: HostOpId,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<VmResult<CallReturn>> {
            std::task::Poll::Pending
        }
    }

    let submitted = Arc::new(Mutex::new(Vec::new()));
    let future = Arc::new(Mutex::new(None));
    let mut vm = Vm::try_new(Program::new(Vec::new(), vec![OpCode::Ret as u8]))
        .expect("test VM construction must not fail");
    vm.set_async_bridge(Box::new(RecordingBridge {
        submitted: Arc::clone(&submitted),
        future: Arc::clone(&future),
    }));

    let outcome = vm
        .submit_host_future(Box::pin(async {
            Ok(HostFutureOutput::returning(CallReturn::one(Value::Int(42))))
        }))
        .expect("host bridge should accept future");
    let CallOutcome::Pending(op_id) = outcome else {
        panic!("async host submission should suspend");
    };

    assert_eq!(*submitted.lock().expect("submitted lock"), vec![op_id]);
    assert!(future.lock().expect("future lock").is_some());
    // The submitted future is a single registered execution-scope operation.
    assert_eq!(vm.host.execution_scope_operation_count(), 1);
}

#[test]
fn async_host_submission_without_driver_fails_without_allocating() {
    let mut vm = Vm::try_new(Program::new(Vec::new(), vec![OpCode::Ret as u8]))
        .expect("test VM construction must not fail");
    let error = vm
        .submit_host_future(Box::pin(async {
            Ok(HostFutureOutput::returning(CallReturn::none()))
        }))
        .expect_err("missing host async driver should fail");

    assert!(
        error
            .to_string()
            .contains("async host function requires a host async bridge")
    );
    // The id space is untouched by a rejected submission: no bridge was
    // present, so no operation (scope or bridge-external) was created.
    assert_eq!(vm.host.execution_scope_operation_count(), 0);
}

#[test]
fn completing_a_submitted_host_op_cancels_the_driver_future() {
    use std::sync::{Arc, Mutex};

    struct CancelRecordingBridge(Arc<Mutex<Vec<HostOpId>>>);

    impl HostAsyncBridge for CancelRecordingBridge {
        fn submit_op(&mut self, _op_id: HostOpId, _future: HostFuture) -> VmResult<()> {
            Ok(())
        }

        fn poll_op(
            &mut self,
            _op_id: HostOpId,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<VmResult<CallReturn>> {
            std::task::Poll::Pending
        }

        fn cancel_op(&mut self, op_id: HostOpId) {
            self.0.lock().expect("cancel lock").push(op_id);
        }
    }

    let cancelled = Arc::new(Mutex::new(Vec::new()));
    let mut vm = Vm::try_new(Program::new(Vec::new(), vec![OpCode::Ret as u8]))
        .expect("test VM construction must not fail");
    vm.set_async_bridge(Box::new(CancelRecordingBridge(Arc::clone(&cancelled))));
    let CallOutcome::Pending(op_id) = vm
        .submit_host_future(Box::pin(async {
            Ok(HostFutureOutput::returning(CallReturn::none()))
        }))
        .expect("future should submit")
    else {
        panic!("submission should return pending");
    };
    vm.set_waiting_host_op(op_id)
        .expect("submitted op should register");

    vm.complete_host_op(op_id, CallReturn::none())
        .expect("manual completion should succeed");

    assert_eq!(*cancelled.lock().expect("cancel lock"), vec![op_id]);
    assert_eq!(vm.waiting_host_op_id(), None);
    assert_eq!(
        vm.host.execution_scope_operation_count(),
        0,
        "external completion must consume and release the terminal slot"
    );
    assert_eq!(
        vm.host.pending_op_results.len(),
        0,
        "external completion must remove the result adapter"
    );
}

#[test]
fn failed_submitted_host_completion_clears_waiting_state() {
    struct FailingCompletionBridge;

    impl HostAsyncBridge for FailingCompletionBridge {
        fn submit_op(&mut self, _op_id: HostOpId, _future: HostFuture) -> VmResult<()> {
            Ok(())
        }

        fn poll_op(
            &mut self,
            _op_id: HostOpId,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<VmResult<CallReturn>> {
            std::task::Poll::Pending
        }

        fn poll_submitted_op(
            &mut self,
            _op_id: HostOpId,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<VmResult<HostFutureOutput>> {
            std::task::Poll::Ready(Ok(HostFutureOutput::complete(|_| {
                Err(VmError::HostError("completion failed".to_string()))
            })))
        }
    }

    let mut vm = Vm::try_new(Program::new(Vec::new(), vec![OpCode::Ret as u8]))
        .expect("test VM construction must not fail");
    vm.set_async_bridge(Box::new(FailingCompletionBridge));
    let CallOutcome::Pending(op_id) = vm
        .submit_host_future(Box::pin(async {
            Ok(HostFutureOutput::returning(CallReturn::none()))
        }))
        .expect("future should submit")
    else {
        panic!("submission should return pending");
    };
    vm.set_waiting_host_op(op_id)
        .expect("submitted op should register");
    let waker = futures_util::task::noop_waker();
    let mut context = std::task::Context::from_waker(&waker);

    let result = vm.poll_waiting_host_op(&mut context);

    assert!(matches!(
        result,
        std::task::Poll::Ready(Err(VmError::HostError(message)))
            if message == "completion failed"
    ));
    assert_eq!(vm.waiting_host_op_id(), None);
    // The failed poll consumed the registered operation's slot and adapter.
    assert_eq!(vm.host.execution_scope_operation_count(), 0);
    assert!(vm.host.pending_op_results.is_empty());
}

#[test]
fn cancelled_submitted_host_poll_retires_slot_and_result_adapter() {
    let mut vm =
        Vm::try_new(Program::new(Vec::new(), vec![OpCode::Ret as u8])).expect("construct VM");
    vm.set_async_bridge(Box::new(NoopPendingBridge));
    let CallOutcome::Pending(raw) = vm
        .submit_host_future(Box::pin(async {
            Ok(HostFutureOutput::returning(CallReturn::none()))
        }))
        .expect("submit pending future")
    else {
        panic!("future should be pending");
    };
    vm.set_waiting_host_op(raw)
        .expect("admit pending operation");
    let id = crate::vm::operation::OperationId::from_raw(raw).unwrap();
    vm.host
        .execution_scope_cancel_operation(
            id,
            crate::vm::operation::OperationCancelReason::Requested,
        )
        .expect("mark operation cancelled");

    let waker = futures_util::task::noop_waker();
    let mut context = std::task::Context::from_waker(&waker);
    let result = vm.poll_waiting_host_op(&mut context);
    assert!(matches!(
        result,
        Poll::Ready(Err(VmError::HostError(message)))
            if message.contains(&format!("host operation {raw} cancelled"))
    ));
    assert_eq!(vm.host.execution_scope_operation_count(), 0);
    assert!(vm.host.pending_op_results.is_empty());
    assert_eq!(vm.waiting_host_op_id(), None);
}

// ---------------------------------------------------------------------------
// Bridge generation ownership: swap/clear with un-awaited pending operations
// ---------------------------------------------------------------------------

/// A bridge that records which generation instance it belongs to and drops
/// into a shared counter when the last `Arc` reference to its generation is
/// released. `poll`/`cancel` record the generation that actually served the
/// call, proving an operation routes to the exact generation it was submitted
/// against even after the VM swaps its current bridge.
struct GenerationBridge {
    generation: u64,
    futures: std::collections::HashMap<HostOpId, HostFuture>,
    served_by: Arc<Mutex<Vec<(HostOpId, &'static str, u64)>>>,
    drops: Arc<AtomicUsize>,
}

impl GenerationBridge {
    fn new(
        generation: u64,
        served_by: Arc<Mutex<Vec<(HostOpId, &'static str, u64)>>>,
        drops: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            generation,
            futures: std::collections::HashMap::new(),
            served_by,
            drops,
        }
    }
}

impl Drop for GenerationBridge {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

impl HostAsyncBridge for GenerationBridge {
    fn submit_op(&mut self, op_id: HostOpId, future: HostFuture) -> VmResult<()> {
        self.futures.insert(op_id, future);
        self.served_by
            .lock()
            .expect("served-by lock")
            .push((op_id, "submit", self.generation));
        Ok(())
    }

    fn poll_op(
        &mut self,
        op_id: HostOpId,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<VmResult<CallReturn>> {
        self.served_by
            .lock()
            .expect("served-by lock")
            .push((op_id, "poll", self.generation));
        std::task::Poll::Pending
    }

    fn poll_submitted_op(
        &mut self,
        op_id: HostOpId,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<VmResult<HostFutureOutput>> {
        self.served_by.lock().expect("served-by lock").push((
            op_id,
            "poll_submitted",
            self.generation,
        ));
        self.futures.get_mut(&op_id).map_or(
            std::task::Poll::Ready(Err(VmError::HostError(format!(
                "unknown submitted host operation {op_id}"
            )))),
            |future| future.as_mut().poll(cx),
        )
    }

    fn cancel_op(&mut self, op_id: HostOpId) {
        self.cancel_op_with_reason(op_id, CancellationReason::Requested);
    }

    fn cancel_op_with_reason(&mut self, op_id: HostOpId, _reason: CancellationReason) {
        self.served_by
            .lock()
            .expect("served-by lock")
            .push((op_id, "cancel", self.generation));
        self.futures.remove(&op_id);
    }
}

fn noop_test_waker() -> std::task::Waker {
    std::task::Waker::from(std::sync::Arc::new(NoopWake))
}

struct NoopWake;

impl std::task::Wake for NoopWake {
    fn wake(self: std::sync::Arc<Self>) {}
}

/// An un-awaited pending bridge operation keeps polling and cancelling against
/// its original bridge generation after `set_async_bridge` swaps in a new
/// generation; new submissions use the new generation. The old generation
/// drops only after its outstanding driver is released.
#[test]
fn pending_bridge_op_survives_swap_and_polls_old_generation() {
    let served_by = Arc::new(Mutex::new(Vec::new()));
    let drops = Arc::new(AtomicUsize::new(0));
    let mut vm = Vm::try_new(Program::new(Vec::new(), vec![OpCode::Ret as u8]))
        .expect("test VM construction must not fail");
    vm.set_async_bridge(Box::new(GenerationBridge::new(
        1,
        Arc::clone(&served_by),
        Arc::clone(&drops),
    )));

    // Submit a pending op and *do not* await it: the driver holds the gen-1
    // Arc clone, keeping generation 1 alive independently of the VM.
    let CallOutcome::Pending(old_op) = vm
        .submit_host_future(Box::pin(std::future::pending()))
        .expect("gen-1 bridge should accept the future")
    else {
        panic!("submission should return pending");
    };

    // Swap the bridge: the VM's current generation becomes 2, but the old
    // op's driver still pins generation 1 (the old bridge is not dropped).
    vm.set_async_bridge(Box::new(GenerationBridge::new(
        2,
        Arc::clone(&served_by),
        Arc::clone(&drops),
    )));
    assert_eq!(
        drops.load(Ordering::SeqCst),
        0,
        "gen-1 bridge must survive while its driver is registered"
    );

    // The old op can still be awaited: polling routes to generation 1.
    vm.set_waiting_host_op(old_op)
        .expect("old op should register as waiting");
    let waker = noop_test_waker();
    let mut cx = std::task::Context::from_waker(&waker);
    assert!(matches!(
        vm.poll_waiting_host_op(&mut cx),
        std::task::Poll::Pending
    ));
    assert_eq!(
        *served_by.lock().expect("served-by lock"),
        vec![(old_op, "submit", 1), (old_op, "poll_submitted", 1)],
        "old op must poll through generation 1"
    );

    // New submissions use the new generation (2).
    let CallOutcome::Pending(new_op) = vm
        .submit_host_future(Box::pin(std::future::pending()))
        .expect("gen-2 bridge should accept the future")
    else {
        panic!("submission should return pending");
    };
    assert_eq!(
        *served_by.lock().expect("served-by lock"),
        vec![
            (old_op, "submit", 1),
            (old_op, "poll_submitted", 1),
            (new_op, "submit", 2),
        ],
        "new op must submit through generation 2"
    );

    // Both generations coexist in the single modern registry.
    assert_eq!(vm.host.execution_scope_operation_count(), 2);

    // Cancelling the old op routes through generation 1 (its own bridge),
    // not the current generation 2.
    vm.try_cancel_waiting_host_op()
        .expect("waiting host operation cancellation should succeed");
    assert_eq!(
        *served_by.lock().expect("served-by lock"),
        vec![
            (old_op, "submit", 1),
            (old_op, "poll_submitted", 1),
            (new_op, "submit", 2),
            (old_op, "cancel", 1),
        ],
        "old op must cancel through generation 1"
    );
    assert_eq!(
        vm.host.execution_scope_operation_count(),
        1,
        "explicit cancellation retires the old slot while the new op remains"
    );

    // Release the new op's driver too: now generation 2's bridge (held only
    // by the VM and the new driver) drops once both are released, and
    // generation 1 drops once its last driver reference is gone.
    vm.set_waiting_host_op(new_op)
        .expect("new op should register as waiting");
    vm.try_cancel_waiting_host_op()
        .expect("waiting host operation cancellation should succeed");
    drop(vm);
    assert_eq!(
        drops.load(Ordering::SeqCst),
        2,
        "both bridge generations must drop after their drivers finish"
    );
}

/// Clearing the bridge while a pending (never-awaited) op is registered keeps
/// the op safe: the op can still be polled/cancelled against its retained
/// generation, and a scope reset cancels it exactly once with no double
/// cancel and no crash.
#[test]
fn clear_bridge_keeps_unawaited_op_cancellable_once() {
    let served_by = Arc::new(Mutex::new(Vec::new()));
    let drops = Arc::new(AtomicUsize::new(0));
    let mut vm = Vm::try_new(Program::new(Vec::new(), vec![OpCode::Ret as u8]))
        .expect("test VM construction must not fail");
    vm.set_async_bridge(Box::new(GenerationBridge::new(
        7,
        Arc::clone(&served_by),
        Arc::clone(&drops),
    )));
    let CallOutcome::Pending(op_id) = vm
        .submit_host_future(Box::pin(std::future::pending()))
        .expect("bridge should accept the future")
    else {
        panic!("submission should return pending");
    };

    // Clear: the VM drops its current generation reference, but the driver's
    // clone keeps the bridge alive.
    vm.clear_async_bridge();
    assert_eq!(
        drops.load(Ordering::SeqCst),
        0,
        "clearing the VM's reference must not drop the generation while a driver is registered"
    );

    // New submissions are rejected after a clear.
    let error = vm
        .submit_host_future(Box::pin(std::future::pending()))
        .expect_err("cleared bridge must reject new submissions");
    assert!(error.to_string().contains("requires a host async bridge"));

    // The retained generation still serves the pending op: it can be awaited
    // and polled safely.
    vm.set_waiting_host_op(op_id)
        .expect("old op should register as waiting");
    let waker = noop_test_waker();
    let mut cx = std::task::Context::from_waker(&waker);
    assert!(matches!(
        vm.poll_waiting_host_op(&mut cx),
        std::task::Poll::Pending
    ));
    assert_eq!(
        *served_by.lock().expect("served-by lock"),
        vec![(op_id, "submit", 7), (op_id, "poll_submitted", 7)],
        "cleared-generation op must still poll through generation 7"
    );

    // A scope reset cancels the pending op exactly once through its retained
    // generation, and the generation drops once the driver is released.
    vm.reset_for_reuse();
    assert_eq!(
        *served_by.lock().expect("served-by lock"),
        vec![
            (op_id, "submit", 7),
            (op_id, "poll_submitted", 7),
            (op_id, "cancel", 7),
        ],
        "reset must cancel the retained-generation op exactly once"
    );
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "generation 7 must drop after its driver is released"
    );
    // The registry is drained by the reset.
    assert_eq!(vm.host.execution_scope_operation_count(), 0);
}

/// A *waiting* op is cancelled exactly once against its original generation
/// when the bridge is swapped: `set_async_bridge` cancels the currently
/// waited-on op (legacy swap semantics) through the generation it belongs to,
/// clears the wait, and installs the new generation. No dangling reference and
/// no double cancel.
#[test]
fn waiting_op_swap_cancels_exactly_once_against_original_generation() {
    let served_by = Arc::new(Mutex::new(Vec::new()));
    let drops = Arc::new(AtomicUsize::new(0));
    let mut vm = Vm::try_new(Program::new(Vec::new(), vec![OpCode::Ret as u8]))
        .expect("test VM construction must not fail");
    vm.set_async_bridge(Box::new(GenerationBridge::new(
        3,
        Arc::clone(&served_by),
        Arc::clone(&drops),
    )));
    let CallOutcome::Pending(op_id) = vm
        .submit_host_future(Box::pin(std::future::pending()))
        .expect("bridge should accept the future")
    else {
        panic!("submission should return pending");
    };
    vm.set_waiting_host_op(op_id)
        .expect("op should register as waiting");

    // Swap while the op is actively waited on: the waiting op is cancelled
    // exactly once through generation 3, then the new generation is installed.
    vm.set_async_bridge(Box::new(GenerationBridge::new(
        4,
        Arc::clone(&served_by),
        Arc::clone(&drops),
    )));
    assert_eq!(
        *served_by.lock().expect("served-by lock"),
        vec![(op_id, "submit", 3), (op_id, "cancel", 3)],
        "waiting op must be cancelled exactly once through generation 3 on swap"
    );
    assert_eq!(vm.waiting_host_op_id(), None);

    // The new generation is live and accepts a fresh submission.
    let CallOutcome::Pending(new_op) = vm
        .submit_host_future(Box::pin(std::future::pending()))
        .expect("gen-4 bridge should accept the future")
    else {
        panic!("submission should return pending");
    };
    assert_eq!(
        *served_by.lock().expect("served-by lock"),
        vec![
            (op_id, "submit", 3),
            (op_id, "cancel", 3),
            (new_op, "submit", 4),
        ],
        "new op must submit through generation 4"
    );

    // Cancelling the new op does not re-cancel the old (waiting) op: the old
    // cancellation already happened exactly once.
    vm.set_waiting_host_op(new_op)
        .expect("new op should register as waiting");
    vm.try_cancel_waiting_host_op()
        .expect("waiting host operation cancellation should succeed");
    assert_eq!(
        *served_by.lock().expect("served-by lock"),
        vec![
            (op_id, "submit", 3),
            (op_id, "cancel", 3),
            (new_op, "submit", 4),
            (new_op, "cancel", 4),
        ],
        "each op cancels exactly once against its own generation"
    );
    drop(vm);
    assert_eq!(
        drops.load(Ordering::SeqCst),
        2,
        "both generations must drop at teardown"
    );
}

/// Multiple operations across two bridge generations coexist in the single
/// modern operation registry without interference: each op polls and cancels
/// against its own generation.
#[test]
fn multiple_generations_coexist_in_one_registry() {
    let served_by = Arc::new(Mutex::new(Vec::new()));
    let drops = Arc::new(AtomicUsize::new(0));
    let mut vm = Vm::try_new(Program::new(Vec::new(), vec![OpCode::Ret as u8]))
        .expect("test VM construction must not fail");
    vm.set_async_bridge(Box::new(GenerationBridge::new(
        10,
        Arc::clone(&served_by),
        Arc::clone(&drops),
    )));
    let CallOutcome::Pending(gen10_a) = vm
        .submit_host_future(Box::pin(std::future::pending()))
        .expect("gen-10 bridge should accept")
    else {
        panic!("submission should return pending");
    };
    let CallOutcome::Pending(gen10_b) = vm
        .submit_host_future(Box::pin(std::future::pending()))
        .expect("gen-10 bridge should accept")
    else {
        panic!("submission should return pending");
    };

    vm.set_async_bridge(Box::new(GenerationBridge::new(
        11,
        Arc::clone(&served_by),
        Arc::clone(&drops),
    )));
    let CallOutcome::Pending(gen11_a) = vm
        .submit_host_future(Box::pin(std::future::pending()))
        .expect("gen-11 bridge should accept")
    else {
        panic!("submission should return pending");
    };

    assert_eq!(
        vm.host.execution_scope_operation_count(),
        3,
        "all generations share one registry"
    );

    // Cancel the gen-10 ops and the gen-11 op; each routes to its own
    // generation and retires immediately.
    for op_id in [gen10_a, gen10_b, gen11_a] {
        vm.set_waiting_host_op(op_id)
            .expect("op should register as waiting");
        vm.try_cancel_waiting_host_op()
            .expect("waiting host operation cancellation should succeed");
    }
    assert_eq!(vm.host.execution_scope_operation_count(), 0);
    assert_eq!(vm.host.pending_op_results.len(), 0);
    let served = served_by.lock().expect("served-by lock");
    let submit_gen: Vec<u64> = served
        .iter()
        .filter(|(_, action, _)| *action == "submit")
        .map(|(_, _, generation)| *generation)
        .collect();
    let cancel_gen: Vec<u64> = served
        .iter()
        .filter(|(_, action, _)| *action == "cancel")
        .map(|(_, _, generation)| *generation)
        .collect();
    assert_eq!(submit_gen, vec![10, 10, 11]);
    assert_eq!(cancel_gen, vec![10, 10, 11]);
    drop(served);
    assert_eq!(
        vm.host.execution_scope_operation_count(),
        0,
        "all cancelled generations retire from the one registry"
    );
    drop(vm);
    assert_eq!(
        drops.load(Ordering::SeqCst),
        2,
        "both generations drop once all their drivers finish"
    );
}

/// The output/result-cell semantics are preserved across a swap: an op whose
/// future resolves after a swap still materializes its produced value through
/// the pending-result adapter, and a poisoned bridge lock surfaces a typed
/// error instead of a panic or a raw-pointer dereference.
#[test]
fn output_semantics_preserved_across_swap_and_poisoned_lock_is_typed() {
    let served_by = Arc::new(Mutex::new(Vec::new()));
    let drops = Arc::new(AtomicUsize::new(0));
    let mut vm = Vm::try_new(Program::new(Vec::new(), vec![OpCode::Ret as u8]))
        .expect("test VM construction must not fail");
    vm.set_async_bridge(Box::new(GenerationBridge::new(
        5,
        Arc::clone(&served_by),
        Arc::clone(&drops),
    )));
    let CallOutcome::Pending(op_id) = vm
        .submit_host_future(Box::pin(async {
            Ok(HostFutureOutput::returning(CallReturn::one(Value::Int(7))))
        }))
        .expect("bridge should accept the future")
    else {
        panic!("submission should return pending");
    };

    // Swap the bridge before the op completes.
    vm.set_async_bridge(Box::new(GenerationBridge::new(
        6,
        Arc::clone(&served_by),
        Arc::clone(&drops),
    )));
    vm.set_waiting_host_op(op_id)
        .expect("op should register as waiting");
    let waker = noop_test_waker();
    let mut cx = std::task::Context::from_waker(&waker);
    // Polling the old op drives its future to Ready through generation 5.
    assert!(matches!(
        vm.poll_waiting_host_op(&mut cx),
        std::task::Poll::Ready(Ok(()))
    ));
    assert_eq!(
        *served_by.lock().expect("served-by lock"),
        vec![(op_id, "submit", 5), (op_id, "poll_submitted", 5)],
        "old op completes through generation 5"
    );
    assert_eq!(vm.waiting_host_op_id(), None);

    // Poisoned-lock mapping: a poisoned generation mutex surfaces a typed
    // VmError (never a panic and never a raw-pointer dereference).
    let poisoned = Arc::new(Mutex::new(Box::new(GenerationBridge::new(
        9,
        Arc::clone(&served_by),
        Arc::clone(&drops),
    )) as Box<dyn HostAsyncBridge>));
    let poisoned_clone = Arc::clone(&poisoned);
    let poisoner = std::thread::spawn(move || {
        let _guard = poisoned_clone.lock().expect("poison lock");
        panic!("deliberate poison");
    });
    let _ = poisoner.join();
    let poisoned_error = super::async_host::with_bridge(&poisoned, |_bridge| {
        // Never reached: the lock is poisoned.
    })
    .expect_err("a poisoned bridge lock must surface a typed error");
    assert!(
        poisoned_error.to_string().contains("poisoned"),
        "poison must map to a typed VmError, got: {poisoned_error}"
    );

    drop(vm);
    // The two generation bridges used above drop exactly once each.
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}

#[test]
fn poisoned_bridge_submit_failure_is_atomic_and_releases_capacity() {
    use std::sync::{Arc, Mutex};

    let mut vm = Vm::try_new(Program::new(Vec::new(), vec![OpCode::Ret as u8]))
        .expect("test VM construction must not fail");

    // Poison the current bridge generation before submitting.
    let poisoned: Arc<Mutex<Box<dyn HostAsyncBridge>>> =
        Arc::new(Mutex::new(Box::new(PendingBridgeForRollback::default())));
    let poisoned_clone = Arc::clone(&poisoned);
    let poisoner = std::thread::spawn(move || {
        let _guard = poisoned_clone.lock().expect("poison lock");
        panic!("deliberate poison");
    });
    let _ = poisoner.join();
    // Install the poisoned generation as the current bridge so the VM
    // submits against the poisoned lock.
    vm.host.async_bridge = Some(poisoned);

    let baseline_active = vm.host.execution_scope().operations().active_count();
    let baseline_len = vm.host.execution_scope().operations().len();
    let capacity = vm.host.execution_scope().operations().max_pending();
    assert_eq!(baseline_active, 0);
    assert_eq!(baseline_len, 0);

    let error = vm
        .submit_host_future(Box::pin(async {
            Ok(HostFutureOutput::returning(CallReturn::none()))
        }))
        .expect_err("a poisoned bridge generation must fail submission");

    assert!(
        error.to_string().contains("poisoned"),
        "poison must map to a typed VmError, got: {error}"
    );
    // The registered operation was rolled back atomically: no occupant, no
    // active operation, no pending-result adapter, no waiting state.
    assert_eq!(vm.host.execution_scope_operation_count(), 0);
    assert_eq!(vm.host.execution_scope().operations().active_count(), 0);
    assert_eq!(vm.host.execution_scope().operations().len(), 0);
    assert!(vm.host.execution_scope().operations().is_empty());
    assert_eq!(vm.waiting_host_op_id(), None);
    // No pending-result adapter was installed for the failed op.
    assert!(
        vm.host.pending_op_results.is_empty(),
        "a failed submission must leave no pending-result adapter"
    );

    // Full capacity remains available: filling to the configured limit must
    // succeed after the failed submission.
    vm.set_async_bridge(Box::new(PendingBridgeForRollback::default()));
    for _ in 0..capacity {
        vm.submit_host_future(Box::pin(std::future::pending()))
            .expect("full capacity must be available after a failed submission");
    }
    assert_eq!(
        vm.host.execution_scope().operations().active_count(),
        capacity
    );
    assert_eq!(vm.host.execution_scope_operation_count(), capacity);
}

#[test]
fn bridge_rejected_submit_failure_is_atomic_and_releases_capacity() {
    use std::sync::{Arc, Mutex};

    struct RejectingBridge {
        cancels: Arc<Mutex<Vec<HostOpId>>>,
    }
    impl HostAsyncBridge for RejectingBridge {
        fn submit_op(&mut self, _op_id: HostOpId, _future: HostFuture) -> VmResult<()> {
            Err(VmError::HostError(
                "bridge rejected the submission".to_string(),
            ))
        }
        fn poll_op(
            &mut self,
            _op_id: HostOpId,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<VmResult<CallReturn>> {
            std::task::Poll::Pending
        }
        fn cancel_op(&mut self, op_id: HostOpId) {
            self.cancels.lock().expect("cancel lock").push(op_id);
        }
    }

    let cancels = Arc::new(Mutex::new(Vec::new()));
    let mut vm = Vm::try_new(Program::new(Vec::new(), vec![OpCode::Ret as u8]))
        .expect("test VM construction must not fail");
    vm.set_async_bridge(Box::new(RejectingBridge {
        cancels: Arc::clone(&cancels),
    }));

    let baseline_active = vm.host.execution_scope().operations().active_count();
    let baseline_len = vm.host.execution_scope().operations().len();
    let capacity = vm.host.execution_scope().operations().max_pending();
    assert_eq!(baseline_active, 0);
    assert_eq!(baseline_len, 0);

    let error = vm
        .submit_host_future(Box::pin(async {
            Ok(HostFutureOutput::returning(CallReturn::none()))
        }))
        .expect_err("a rejecting bridge must fail submission");
    assert!(
        error.to_string().contains("bridge rejected the submission"),
        "rejection must surface the bridge error, got: {error}"
    );

    // The registered operation was rolled back atomically.
    assert_eq!(vm.host.execution_scope_operation_count(), 0);
    assert_eq!(vm.host.execution_scope().operations().active_count(), 0);
    assert_eq!(vm.host.execution_scope().operations().len(), 0);
    assert!(vm.host.execution_scope().operations().is_empty());
    assert_eq!(vm.waiting_host_op_id(), None);
    // No pending-result adapter was installed for the failed op.
    assert!(
        vm.host.pending_op_results.is_empty(),
        "a rejected submission must leave no pending-result adapter"
    );

    // The driver was cancelled exactly once with the failed op's id.
    let cancels = cancels.lock().expect("cancel lock");
    assert_eq!(
        cancels.len(),
        1,
        "the registered driver must be cancelled exactly once"
    );
    drop(cancels);

    // Full capacity remains available after the failed submission.
    vm.set_async_bridge(Box::new(PendingBridgeForRollback::default()));
    for _ in 0..capacity {
        vm.submit_host_future(Box::pin(std::future::pending()))
            .expect("full capacity must be available after a rejected submission");
    }
    assert_eq!(
        vm.host.execution_scope().operations().active_count(),
        capacity
    );
}

/// A bridge that parks submitted futures and never completes them; used to
/// prove that capacity freed by a failed submission can be refilled.
#[derive(Default)]
struct PendingBridgeForRollback {
    futures: std::collections::HashMap<HostOpId, HostFuture>,
}

impl HostAsyncBridge for PendingBridgeForRollback {
    fn submit_op(&mut self, op_id: HostOpId, future: HostFuture) -> VmResult<()> {
        self.futures.insert(op_id, future);
        Ok(())
    }
    fn poll_op(
        &mut self,
        _op_id: HostOpId,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<VmResult<CallReturn>> {
        std::task::Poll::Pending
    }
    fn poll_submitted_op(
        &mut self,
        op_id: HostOpId,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<VmResult<HostFutureOutput>> {
        self.futures.get_mut(&op_id).map_or(
            std::task::Poll::Ready(Err(VmError::HostError(format!(
                "unknown submitted host operation {op_id}"
            )))),
            |future| future.as_mut().poll(cx),
        )
    }
    fn cancel_op(&mut self, op_id: HostOpId) {
        self.futures.remove(&op_id);
    }
}

#[test]
fn shared_capture_cell_rejects_callable_ownership_cycle() {
    let mut vm = Vm::try_new(Program::new(Vec::new(), vec![OpCode::Ret as u8]).with_local_count(1))
        .expect("test VM construction must not fail");
    let cell = Arc::new(Mutex::new(Value::Null));
    vm.instance.capture_cells.insert(0, Arc::clone(&cell));
    let environment = Arc::new(crate::CallableEnvironment {
        cells: Mutex::new(vec![cell]),
    });
    let callable = Value::Callable(Arc::new(crate::CallableValue {
        prototype_id: 0,
        kind: crate::CallableKind::Closure,
        env: Some(environment),
    }));
    assert!(matches!(
        vm.store_local_with_drop_contract(0, callable),
        Err(VmError::InvalidFrameState(
            "callable capture ownership cycle is unsupported"
        ))
    ));
    assert_eq!(vm.locals()[0], Value::Null);
}

#[test]
fn inline_callable_identity_requires_capture_free_function_item_state() {
    let mut vm = Vm::try_new(Program::new(Vec::new(), vec![OpCode::Ret as u8]).with_local_count(1))
        .expect("test VM construction must not fail");
    let malformed_environment = Arc::new(crate::CallableEnvironment {
        cells: Mutex::new(vec![Arc::new(Mutex::new(Value::Int(7)))]),
    });
    vm.set_local(
        0,
        Value::Callable(Arc::new(crate::CallableValue {
            prototype_id: 42,
            kind: crate::CallableKind::FunctionItem,
            env: Some(malformed_environment),
        })),
    )
    .expect("install malformed callable");
    assert_eq!(vm.active_local_callable_prototypes(), Some(vec![None]));

    vm.set_local(
        0,
        Value::Callable(Arc::new(crate::CallableValue {
            prototype_id: 42,
            kind: crate::CallableKind::Closure,
            env: None,
        })),
    )
    .expect("install closure-shaped callable");
    assert_eq!(vm.active_local_callable_prototypes(), Some(vec![None]));

    vm.set_local(
        0,
        Value::Callable(Arc::new(crate::CallableValue {
            prototype_id: 42,
            kind: crate::CallableKind::FunctionItem,
            env: None,
        })),
    )
    .expect("install inline-compatible callable");
    assert_eq!(vm.active_local_callable_prototypes(), Some(vec![Some(42)]));
}

#[test]
fn callable_operand_type_hint_roundtrips() {
    let packed = pack_operand_types(ValueType::Callable, ValueType::Callable);
    assert_eq!(
        unpack_operand_types(packed),
        (ValueType::Callable, ValueType::Callable)
    );
}

#[test]
fn callvalue_decodes_its_arity_before_callable_validation() {
    let mut vm = Vm::try_new(Program::new(
        Vec::new(),
        vec![OpCode::CallValue as u8, 0, OpCode::Ret as u8],
    ))
    .expect("test VM construction must not fail");
    vm.instance.stack.push(Value::Null);
    assert!(matches!(vm.run(), Err(VmError::InvalidCallable)));
    assert_eq!(vm.ip(), 2);
}

#[test]
fn callvalue_enters_script_frame_and_resumes_caller() {
    let mut bc = crate::BytecodeBuilder::new();
    bc.ldloc(0);
    bc.ldc(0);
    bc.call_value(1);
    bc.ret();
    let function_entry = bc.position();
    bc.ldloc(0);
    bc.ldc(1);
    bc.add();
    bc.ret();
    let function_end = bc.position();

    let program = Program::new(vec![Value::Int(41), Value::Int(1)], bc.finish())
        .with_local_count(1)
        .with_callable_metadata(
            vec![crate::ScriptFunction {
                entry_ip: function_entry,
                end_ip: function_end,
            }],
            vec![crate::CallablePrototype {
                kind: crate::CallableKind::FunctionItem,
                target: crate::CallableTarget::ScriptFunction(0),
                arity: 1,
                frame_local_count: 1,
                parameter_slots: vec![0],
                capture_source_slots: Vec::new(),
                capture_slots: Vec::new(),
                capture_modes: Vec::new(),
                self_slot: None,
                schema: None,
            }],
            vec![
                crate::FunctionRegion {
                    start_ip: 0,
                    end_ip: function_entry,
                    prototype_id: None,
                },
                crate::FunctionRegion {
                    start_ip: function_entry,
                    end_ip: function_end,
                    prototype_id: Some(0),
                },
            ],
            vec![crate::RootCallableBinding {
                local_slot: 0,
                prototype_id: 0,
            }],
        );
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");

    assert_eq!(vm.run().expect("script call should run"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
    assert_eq!(vm.call_depth(), 0);
}

#[test]
fn script_call_depth_limit_is_configurable() {
    let compiled = crate::compile_source_for_repl(
        "fn recurse(value: int) -> int { recurse(value) } recurse(1);",
    )
    .expect("recursive callable should compile");
    let mut vm = Vm::try_new(compiled.program.with_local_count(compiled.locals))
        .expect("test VM construction must not fail");

    assert_eq!(vm.max_script_call_depth(), 1024);
    assert!(matches!(
        vm.set_max_script_call_depth(0),
        Err(VmError::InvalidCallStackLimit(0))
    ));
    vm.set_max_script_call_depth(3)
        .expect("positive call depth should be accepted");
    assert_eq!(vm.max_script_call_depth(), 3);
    assert!(matches!(
        vm.run(),
        Err(VmError::CallStackOverflow { limit: 3 })
    ));
}

#[test]
fn host_can_invoke_exported_callable_and_reset_rebinds_program_owned_value() {
    let mut bc = crate::BytecodeBuilder::new();
    bc.ret();
    let entry = bc.position();
    bc.ldloc(0);
    bc.ldc(0);
    bc.add();
    bc.ret();
    let end = bc.position();
    let program = Program::new(vec![Value::Int(1)], bc.finish())
        .with_local_count(1)
        .with_callable_metadata(
            vec![crate::ScriptFunction {
                entry_ip: entry,
                end_ip: end,
            }],
            vec![crate::CallablePrototype {
                kind: crate::CallableKind::FunctionItem,
                target: crate::CallableTarget::ScriptFunction(0),
                arity: 1,
                frame_local_count: 1,
                parameter_slots: vec![0],
                capture_source_slots: Vec::new(),
                capture_slots: Vec::new(),
                capture_modes: Vec::new(),
                self_slot: None,
                schema: None,
            }],
            vec![
                crate::FunctionRegion {
                    start_ip: 0,
                    end_ip: entry,
                    prototype_id: None,
                },
                crate::FunctionRegion {
                    start_ip: entry,
                    end_ip: end,
                    prototype_id: Some(0),
                },
            ],
            vec![crate::RootCallableBinding {
                local_slot: 0,
                prototype_id: 0,
            }],
        );
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    let callable = vm.locals()[0].clone();
    assert_eq!(vm.run().expect("root should halt"), VmStatus::Halted);
    assert_eq!(
        vm.invoke_callable(callable.clone(), &[Value::Int(41)])
            .expect("host invocation should return"),
        Value::Int(42)
    );
    vm.queue_callable(callable.clone(), vec![Value::Int(1)])
        .expect("queue first callback");
    vm.queue_callable(callable.clone(), vec![Value::Int(2)])
        .expect("queue second callback");
    assert_eq!(vm.queued_callable_count(), 2);
    assert_eq!(
        vm.drain_callable_queue().expect("drain callbacks"),
        vec![Value::Int(2), Value::Int(3)]
    );
    vm.queue_callable(callable.clone(), vec![Value::Int(3)])
        .expect("queue callback before shutdown");
    vm.shutdown();
    assert_eq!(vm.queued_callable_count(), 0);
    assert!(matches!(
        vm.invoke_callable(callable.clone(), &[Value::Int(1)]),
        Err(VmError::InvalidFrameState("vm is shut down"))
    ));

    vm.reset_for_reuse();
    assert_eq!(vm.run().expect("reset root should halt"), VmStatus::Halted);
    let rebound = vm.locals()[0].clone();
    assert_eq!(
        vm.invoke_callable(rebound, &[Value::Int(1)])
            .expect("reset should rebind the Program-owned function item"),
        Value::Int(2)
    );
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn aot_executes_move_detach_without_stack_contract_mismatch() {
    let compiled = crate::compile_source_for_repl(
        r#"
            let source = "x";
            let moved = source;
            moved;
        "#,
    )
    .expect("move source should compile");
    let mut vm = Vm::try_new(compiled.program.with_local_count(compiled.locals))
        .expect("test VM construction must not fail");
    vm.compile_aot().expect("aot compilation should succeed");
    assert_eq!(
        vm.run().expect("aot execution should halt"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::String(Arc::new("x".to_string()))]);
    assert!(!vm.engine.aot_interpreter_boundary_hit);
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn aot_executes_script_callable_frames_without_interpreter_boundary() {
    let compiled = crate::compile_source_for_repl(
        r#"
            fn add_one(value: int) -> int { value + 1 }
            let f = add_one;
            add_one(41);
        "#,
    )
    .expect("script frame source should compile");
    let mut vm = Vm::try_new(compiled.program.with_local_count(compiled.locals))
        .expect("test VM construction must not fail");
    vm.compile_aot().expect("aot compilation should succeed");
    assert_eq!(
        vm.run().expect("aot execution should halt"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(42)]);
    assert!(vm.aot_exec_count() >= 3);
    assert!(!vm.engine.aot_interpreter_boundary_hit);
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn aot_executes_typed_script_callable_parameter_equality_without_interpreter_boundary() {
    let compiled = crate::compile_source(
        r#"
            fn is_zero(value: int) -> bool { value == 0 }
            let f = is_zero;
            is_zero(0);
        "#,
    )
    .expect("typed equality source should compile");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    vm.compile_aot().expect("aot compilation should succeed");
    assert_eq!(
        vm.run().expect("aot execution should halt"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Bool(true)]);
    assert!(!vm.engine.aot_interpreter_boundary_hit);
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn aot_executes_script_callable_bool_return_in_branch_without_interpreter_boundary() {
    let compiled = crate::compile_source(
        r#"
            fn is_zero(value: int) -> bool { value == 0 }
            let f = is_zero;
            let selected = if is_zero(0) => { 1 } else => { 2 };
            selected;
        "#,
    )
    .expect("typed branch source should compile");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    vm.compile_aot().expect("aot compilation should succeed");
    assert_eq!(
        vm.run().expect("aot execution should halt"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(1)]);
    assert!(!vm.engine.aot_interpreter_boundary_hit);
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn aot_executes_capturing_closure_without_interpreter_boundary() {
    let compiled = crate::compile_source_for_repl(
        r#"
            let answer = 42;
            let get_answer = || answer;
            get_answer();
        "#,
    )
    .expect("closure source should compile");
    let mut vm = Vm::try_new(compiled.program.with_local_count(compiled.locals))
        .expect("test VM construction must not fail");
    vm.compile_aot().expect("aot compilation should succeed");
    assert_eq!(
        vm.run().expect("aot execution should halt"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(42)]);
    assert!(vm.aot_exec_count() >= 3);
    assert!(!vm.engine.aot_interpreter_boundary_hit);
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn aot_executes_builtin_callable_values_without_interpreter_boundary() {
    let compiled = crate::compile_source_for_repl(
        r#"
            let function = len;
            function("abc");
        "#,
    )
    .expect("builtin callable source should compile");
    let mut vm = Vm::try_new(compiled.program.with_local_count(compiled.locals))
        .expect("test VM construction must not fail");
    vm.compile_aot().expect("aot compilation should succeed");
    assert_eq!(
        vm.run().expect("aot execution should halt"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(3)]);
    assert!(!vm.engine.aot_interpreter_boundary_hit);
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn aot_callable_call_resumes_after_fuel_yield_without_interpreter_boundary() {
    let compiled = crate::compile_source_for_repl(
        r#"
            fn add_one(value: int) -> int { value + 1 }
            let f = add_one;
            add_one(41);
        "#,
    )
    .expect("callable source should compile");
    let mut vm = Vm::try_new(compiled.program.with_local_count(compiled.locals))
        .expect("test VM construction must not fail");
    vm.compile_aot().expect("aot compilation should succeed");
    vm.set_fuel(0);
    assert_eq!(
        vm.run().expect("fuel exhaustion should yield"),
        VmStatus::Yielded
    );
    assert_eq!(vm.last_yield_reason(), Some(VmYieldReason::Fuel));
    vm.set_fuel(100);
    assert_eq!(
        vm.resume().expect("aot callable should resume"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(42)]);
    assert!(!vm.engine.aot_interpreter_boundary_hit);
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn aot_executes_nested_script_callables_without_interpreter_boundary() {
    let compiled = crate::compile_source_for_repl(
        r#"
            fn inc(value: int) -> int { value + 1 }
            fn twice(value: int) -> int { inc(inc(value)) }
            let f = inc;
            let g = twice;
            twice(40);
        "#,
    )
    .expect("nested callable source should compile");
    let mut vm = Vm::try_new(compiled.program.with_local_count(compiled.locals))
        .expect("test VM construction must not fail");
    vm.compile_aot().expect("aot compilation should succeed");
    assert_eq!(
        vm.run().expect("nested aot call should halt"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(42)]);
    assert!(!vm.engine.aot_interpreter_boundary_hit);
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn aot_recursive_script_callable_reports_depth_limit_without_interpreter_boundary() {
    let compiled = crate::compile_source_for_repl(
        r#"
            fn recurse(value: int) -> int { recurse(value) }
            let f = recurse;
            recurse(1);
        "#,
    )
    .expect("recursive callable source should compile");
    let mut vm = Vm::try_new(compiled.program.with_local_count(compiled.locals))
        .expect("test VM construction must not fail");
    vm.compile_aot().expect("aot compilation should succeed");
    assert!(matches!(
        vm.run(),
        Err(VmError::CallStackOverflow { limit: 1024 })
    ));
    assert!(!vm.engine.aot_interpreter_boundary_hit);
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn aot_host_callable_value_waits_and_resumes_without_interpreter_boundary() {
    struct PendingAotHost;

    impl HostFunction for PendingAotHost {
        fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
            // A real execution-scope operation (bridge-submitted future)
            // rather than a fabricated id: every production pending host
            // operation lives in the scope registry.
            vm.submit_host_future(Box::pin(std::future::pending()))
        }
    }

    let compiled = crate::compile_source_for_repl(
        r#"
            fn action(value: int) -> int;
            let function = action;
            function(41);
        "#,
    )
    .expect("host callable source should compile");
    let mut vm = Vm::try_new(compiled.program.with_local_count(compiled.locals))
        .expect("test VM construction must not fail");
    vm.set_async_bridge(Box::new(NoopPendingBridge));
    vm.register_function(Box::new(PendingAotHost));
    vm.compile_aot().expect("aot compilation should succeed");
    let VmStatus::Waiting(op_id) = vm
        .run()
        .expect("pending host callable should wait through the scope registry")
    else {
        panic!("pending host callable should wait");
    };
    assert!(!vm.engine.aot_interpreter_boundary_hit);
    vm.complete_host_op(op_id, vec![Value::Int(42)])
        .expect("host operation should complete");
    assert_eq!(
        vm.resume().expect("aot host callable should resume"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(42)]);
    assert!(!vm.engine.aot_interpreter_boundary_hit);
}

#[test]
fn typed_script_callbacks_invoke_queue_unsubscribe_and_invalidate() {
    let compiled = crate::compile_source_for_repl(
        r#"
            fn add_one(value: int) -> int { value + 1 }
            add_one;
        "#,
    )
    .expect("callback source should compile");
    let mut vm = Vm::try_new(compiled.program.with_local_count(compiled.locals))
        .expect("test VM construction must not fail");
    assert_eq!(vm.run().expect("root should halt"), VmStatus::Halted);
    let callable = vm.stack().last().cloned().expect("callable result");
    let mut store = crate::Store::from_vm(vm);
    let callback: crate::ScriptCallback<(i64,), i64> = store
        .script_callback(callable.clone())
        .expect("typed callback should bind");

    assert_eq!(callback.call(&mut store, (41,)).expect("direct call"), 42);
    let queued = callback.prepare((40,)).expect("queued call should prepare");
    let queued = std::thread::spawn(move || queued)
        .join()
        .expect("queued invocation should cross threads");
    store
        .enqueue_callback(queued)
        .expect("queued call should bind to its store");
    assert_eq!(
        store.drain_callbacks().expect("queue should drain"),
        vec![Value::Int(41)]
    );

    let alias = callback.clone();
    assert!(matches!(
        store.script_callback::<(bool,), i64>(callable.clone()),
        Err(VmError::TypeMismatch("script callback argument schema"))
    ));
    assert!(matches!(
        store.script_callback::<(i64,), bool>(callable.clone()),
        Err(VmError::TypeMismatch("script callback result schema"))
    ));

    callback.unsubscribe();
    assert!(!alias.is_subscribed());
    assert!(matches!(
        alias.prepare((1,)),
        Err(VmError::InvalidFrameState(
            "script callback is unsubscribed"
        ))
    ));

    let independently_subscribed: crate::ScriptCallback<(i64,), i64> = store
        .script_callback(callable)
        .expect("second callback should bind");
    let queued_before_unsubscribe = independently_subscribed
        .prepare((1,))
        .expect("active callback should prepare");
    independently_subscribed.unsubscribe();
    assert!(matches!(
        store.enqueue_callback(queued_before_unsubscribe),
        Err(VmError::InvalidFrameState(
            "script callback is unsubscribed"
        ))
    ));
}

#[test]
fn callback_unsubscribe_cancels_already_enqueued_work() {
    let compiled = crate::compile_source_for_repl(
        r#"
            fn add_one(value: int) -> int { value + 1 }
            add_one;
        "#,
    )
    .expect("callback source should compile");
    let mut vm = Vm::try_new(compiled.program.with_local_count(compiled.locals))
        .expect("test VM construction must not fail");
    assert_eq!(vm.run().expect("root should halt"), VmStatus::Halted);
    let callable = vm.stack().last().cloned().expect("callable result");
    let mut store = crate::Store::from_vm(vm);
    let callback: crate::ScriptCallback<(i64,), i64> = store
        .script_callback(callable)
        .expect("callback should bind");
    let queued = callback.prepare((41,)).expect("callback should prepare");
    store
        .enqueue_callback(queued)
        .expect("callback should enqueue");
    callback.unsubscribe();
    assert_eq!(
        store
            .drain_callbacks()
            .expect("canceled queue should drain"),
        Vec::<Value>::new()
    );
}

#[test]
fn store_reset_and_replacement_invalidate_callback_registries() {
    let compiled = crate::compile_source_for_repl(
        r#"
            fn add_one(value: int) -> int { value + 1 }
            add_one;
        "#,
    )
    .expect("first callback source should compile");
    let mut vm = Vm::try_new(compiled.program.with_local_count(compiled.locals))
        .expect("test VM construction must not fail");
    assert_eq!(vm.run().expect("first root should halt"), VmStatus::Halted);
    let callable = vm.stack().last().cloned().expect("first callable result");
    let mut store = crate::Store::from_vm(vm);
    let callback: crate::ScriptCallback<(i64,), i64> = store
        .script_callback(callable)
        .expect("first callback should bind");
    let prepared = callback.prepare((1,)).expect("callback should prepare");

    store.reset_for_reuse();
    assert!(!callback.is_subscribed());
    assert!(matches!(
        store.enqueue_callback(prepared),
        Err(VmError::InvalidFrameState(
            "script callback belongs to another store"
        ))
    ));

    let replacement = crate::compile_source_for_repl(
        r#"
            fn double(value: int) -> int { value * 2 }
            double;
        "#,
    )
    .expect("replacement callback source should compile");
    let mut replacement_vm = Vm::try_new(replacement.program.with_local_count(replacement.locals))
        .expect("test VM construction must not fail");
    assert_eq!(
        replacement_vm.run().expect("replacement root should halt"),
        VmStatus::Halted
    );
    let replacement_callable = replacement_vm
        .stack()
        .last()
        .cloned()
        .expect("replacement callable result");
    store.replace_vm(replacement_vm);
    let replacement_callback: crate::ScriptCallback<(i64,), i64> = store
        .script_callback(replacement_callable)
        .expect("replacement callback should bind");
    assert_eq!(
        replacement_callback
            .call(&mut store, (21,))
            .expect("replacement callback should run"),
        42
    );
}

#[test]
fn synchronous_callback_error_unwinds_before_next_invocation() {
    let compiled = crate::compile_source_for_repl(
        r#"
            fn fail() -> int { 1 / 0 }
            fn answer() -> int { 42 }
            fail;
            answer;
        "#,
    )
    .expect("callback error source should compile");
    let mut vm = Vm::try_new(compiled.program.with_local_count(compiled.locals))
        .expect("test VM construction must not fail");
    assert_eq!(vm.run().expect("root should halt"), VmStatus::Halted);
    let fail_callable = vm.stack()[0].clone();
    let answer_callable = vm.stack()[1].clone();
    let mut store = crate::Store::from_vm(vm);
    let fail: crate::ScriptCallback<(), i64> = store
        .script_callback(fail_callable)
        .expect("failing callback should bind");
    let answer: crate::ScriptCallback<(), i64> = store
        .script_callback(answer_callable)
        .expect("answer callback should bind");

    assert!(matches!(
        fail.call(&mut store, ()),
        Err(VmError::DivisionByZero)
    ));
    assert_eq!(store.vm().call_depth(), 0);
    assert!(store.vm().execution_frames().is_empty());
    assert_eq!(
        answer
            .call(&mut store, ())
            .expect("next callback should run without reset"),
        42
    );
}

#[test]
fn final_script_callback_releases_capture_environment_once() {
    let compiled = crate::compile_source_for_repl(
        r#"
            fn make_callback() {
                let captured = 42;
                || captured
            }
            make_callback();
        "#,
    )
    .expect("capturing callback source should compile");
    let mut vm = Vm::try_new(compiled.program.with_local_count(compiled.locals))
        .expect("test VM construction must not fail");
    assert_eq!(vm.run().expect("root should halt"), VmStatus::Halted);
    let callable = vm.stack().last().cloned().expect("capturing callback");
    let Value::Callable(callable_value) = &callable else {
        panic!("expected callable value");
    };
    let environment = callable_value
        .env
        .as_ref()
        .expect("capturing callback should own an environment");
    let weak_environment = Arc::downgrade(environment);
    let mut store = crate::Store::from_vm(vm);
    let callback: crate::ScriptCallback<(), i64> = store
        .script_callback(callable.clone())
        .expect("capturing callback should bind");

    store.vm_mut().shutdown();
    drop(callable);
    drop(store);
    assert!(weak_environment.upgrade().is_some());
    drop(callback);
    assert!(weak_environment.upgrade().is_none());
}

#[test]
fn store_resolves_only_exported_script_functions_by_name() {
    let compiled = crate::compile_source_for_repl(
        r#"
            pub fn add_one(value: int) -> int { value + 1 }
            fn private_double(value: int) -> int { value * 2 }
        "#,
    )
    .expect("exported callback source should compile");
    let mut vm = Vm::try_new(compiled.program.with_local_count(compiled.locals))
        .expect("test VM construction must not fail");
    assert_eq!(vm.run().expect("root should halt"), VmStatus::Halted);
    let mut store = crate::Store::from_vm(vm);
    let callback: crate::ScriptCallback<(i64,), i64> = store
        .script_callback_by_name("add_one")
        .expect("exported callback should resolve");
    assert_eq!(callback.call(&mut store, (41,)).expect("export call"), 42);
    assert!(matches!(
        store.resolve_exported_callable("private_double"),
        Err(VmError::HostError(_))
    ));
}

#[test]
fn store_rejects_callable_values_from_another_store() {
    let first =
        crate::compile_source_for_repl("pub fn value() -> int { 11 }").expect("first store source");
    let mut first_store = crate::Store::from_vm(
        Vm::try_new(first.program.with_local_count(first.locals))
            .expect("test VM construction must not fail"),
    );
    assert_eq!(first_store.run().expect("first root"), VmStatus::Halted);
    let foreign = first_store
        .resolve_exported_callable("value")
        .expect("first export");

    let second = crate::compile_source_for_repl("pub fn value() -> int { 22 }")
        .expect("second store source");
    let mut second_store = crate::Store::from_vm(
        Vm::try_new(second.program.with_local_count(second.locals))
            .expect("test VM construction must not fail"),
    );
    assert_eq!(second_store.run().expect("second root"), VmStatus::Halted);
    let injected_slot = u8::try_from(second_store.vm().program().exported_callables[0].local_slot)
        .expect("test slot fits u8");
    second_store
        .vm_mut()
        .set_local(injected_slot, foreign.clone())
        .expect("foreign value can be injected into raw VM state");
    assert!(matches!(
        second_store.script_callback::<(), i64>(foreign),
        Err(VmError::InvalidFrameState(
            "script callable does not belong to this store"
        ))
    ));
}

#[test]
fn callback_queue_preserves_completed_results_and_remaining_events_after_error() {
    let compiled = crate::compile_source_for_repl(
        r#"
            pub fn first() -> int { 1 }
            pub fn fail() -> int { 1 / 0 }
            pub fn third() -> int { 3 }
        "#,
    )
    .expect("queue source");
    let mut store = crate::Store::from_vm(
        Vm::try_new(compiled.program.with_local_count(compiled.locals))
            .expect("test VM construction must not fail"),
    );
    assert_eq!(store.run().expect("queue root"), VmStatus::Halted);
    let first: crate::ScriptCallback<(), i64> = store.script_callback_by_name("first").unwrap();
    let fail: crate::ScriptCallback<(), i64> = store.script_callback_by_name("fail").unwrap();
    let third: crate::ScriptCallback<(), i64> = store.script_callback_by_name("third").unwrap();
    store.enqueue_callback(first.prepare(()).unwrap()).unwrap();
    store.enqueue_callback(fail.prepare(()).unwrap()).unwrap();
    store.enqueue_callback(third.prepare(()).unwrap()).unwrap();

    assert!(matches!(
        store.drain_callbacks(),
        Err(VmError::DivisionByZero)
    ));
    assert_eq!(store.take_callback_result::<i64>().unwrap(), Some(1));
    assert_eq!(store.vm().queued_callable_count(), 1);
    assert_eq!(store.drain_callbacks().unwrap(), vec![Value::Int(3)]);
}

#[test]
fn typed_script_callback_can_wait_resume_and_return_to_host() {
    struct PendingCallbackHost;

    impl HostFunction for PendingCallbackHost {
        fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
            // A real execution-scope operation (bridge-submitted future)
            // rather than a fabricated id: every production pending host
            // operation lives in the scope registry.
            vm.submit_host_future(Box::pin(std::future::pending()))
        }
    }

    let compiled = crate::compile_source_for_repl(
        r#"
            fn wait();
            fn callback() -> int {
                wait();
                42;
            }
            callback;
        "#,
    )
    .expect("callback source should compile");
    let mut vm = Vm::try_new(compiled.program.with_local_count(compiled.locals))
        .expect("test VM construction must not fail");
    vm.set_async_bridge(Box::new(NoopPendingBridge));
    vm.register_function(Box::new(PendingCallbackHost));
    assert_eq!(vm.run().expect("root should halt"), VmStatus::Halted);
    let callable = vm.stack().last().cloned().expect("callable result");
    let mut store = crate::Store::from_vm(vm);
    let callback: crate::ScriptCallback<(), i64> = store
        .script_callback(callable)
        .expect("typed callback should bind");

    let VmStatus::Waiting(op_id) = callback
        .start(&mut store, ())
        .expect("callback should start through the scope registry")
    else {
        panic!("callback should wait on a real scope operation");
    };
    assert_eq!(store.vm().call_depth(), 1);
    store
        .vm_mut()
        .complete_host_op(op_id, Vec::new())
        .expect("host completion should succeed");
    assert_eq!(
        store.resume().expect("callback should resume"),
        VmStatus::Halted
    );
    assert_eq!(store.vm().call_depth(), 0);
    assert_eq!(
        store
            .take_callback_result::<i64>()
            .expect("typed callback result")
            .expect("callback should produce a result"),
        42
    );
}

#[test]
fn typed_script_callback_can_yield_resume_and_return_to_host() {
    let compiled = crate::compile_source_for_repl(
        r#"
            fn sum_to(limit: int) -> int {
                let mut index = 0;
                let mut total = 0;
                while index < limit {
                    total = total + index;
                    index = index + 1;
                }
                total;
            }
            sum_to;
        "#,
    )
    .expect("callback source should compile");
    let mut vm = Vm::try_new(compiled.program.with_local_count(compiled.locals))
        .expect("test VM construction must not fail");
    assert_eq!(vm.run().expect("root should halt"), VmStatus::Halted);
    let callable = vm.stack().last().cloned().expect("callable result");
    let mut store = crate::Store::from_vm(vm);
    let callback: crate::ScriptCallback<(i64,), i64> = store
        .script_callback(callable)
        .expect("typed callback should bind");

    store.set_fuel(4);
    let mut status = callback
        .start(&mut store, (100,))
        .expect("callback should start");
    assert_eq!(store.vm().call_depth(), 1);
    let mut yields = 0usize;
    loop {
        match status {
            VmStatus::Halted => break,
            VmStatus::Yielded => {
                yields += 1;
                assert!(yields < 1_000, "callback should make progress");
                store.recharge(4).expect("fuel recharge should succeed");
                status = store.resume().expect("callback should resume");
            }
            VmStatus::Waiting(_) => panic!("unexpected waiting callback"),
        }
    }
    assert!(yields > 0);
    assert_eq!(store.vm().call_depth(), 0);
    assert_eq!(
        store
            .take_callback_result::<i64>()
            .expect("typed callback result")
            .expect("callback should produce a result"),
        4_950
    );
}

#[test]
fn vm_instances_share_decoded_instruction_metadata_across_program_clones() {
    let compiled = crate::compile_source(
        r#"
        let mut i = 0;
        let mut sum = 0;
        while i < 16 {
            let a = i + 7;
            let b = a - 3;
            sum = sum + b;
            i = i + 1;
        }
        sum;
    "#,
    )
    .expect("source should compile");

    let base_program = compiled.program.with_local_count(compiled.locals.max(8));
    let vm_one = Vm::try_new(
        base_program
            .clone()
            .with_local_count(base_program.local_count + 8),
    )
    .expect("test VM construction must not fail");
    let vm_two = Vm::try_new(base_program.with_local_count(compiled.locals.max(8) + 16))
        .expect("test VM construction must not fail");

    assert!(
        Arc::ptr_eq(
            &vm_one.engine.decoded_instruction_data,
            &vm_two.engine.decoded_instruction_data
        ),
        "program clones should share decoded instruction metadata"
    );
}

#[test]
fn borrowed_map_iterator_state_is_released_after_break() {
    let compiled = crate::compile_source_with_flavor(
        r#"
        let values: map<int> = {a: 1, b: 2};
        for (key: string, value: int) in &values {
            key;
            value;
            break;
        }
        values;
        "#,
        crate::SourceFlavor::RustScript,
    )
    .expect("source should compile");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");

    assert_eq!(vm.run().expect("vm should run"), VmStatus::Halted);
    assert!(
        vm.instance
            .map_iterators
            .iter()
            .flatten()
            .all(Option::is_none),
        "break must release every iterator owned by the exited loop"
    );
}

#[test]
fn borrowed_map_iterator_state_is_released_after_runtime_error() {
    let compiled = crate::compile_source_with_flavor(
        r#"
        let values: map<int> = {a: 1};
        let zero: int = 0;
        for (key: string, value: int) in &values {
            let failure: int = 1 / zero;
        }
        "#,
        crate::SourceFlavor::RustScript,
    )
    .expect("source should compile");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");

    vm.run().expect_err("program should fail at runtime");
    assert!(
        vm.instance
            .map_iterators
            .iter()
            .flatten()
            .all(Option::is_none),
        "runtime errors must release active map iterators"
    );
}

#[test]
fn map_iterator_ids_are_isolated_by_call_depth() {
    let program = Program::new(Vec::new(), vec![OpCode::Ret as u8]);
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    let Value::Map(outer) = Value::map(vec![(Value::string("outer"), Value::Int(1))]) else {
        unreachable!();
    };
    let Value::Map(inner) = Value::map(vec![(Value::string("inner"), Value::Int(2))]) else {
        unreachable!();
    };

    vm.init_map_iterator(7, outer).expect("outer init");
    vm.instance.call_depth = 1;
    vm.init_map_iterator(7, inner).expect("inner init");
    assert!(vm.advance_map_iterator(7).expect("inner advance"));
    assert_eq!(
        vm.take_map_iterator_key(7).expect("inner key"),
        Value::string("inner")
    );
    vm.close_map_iterator(7).expect("inner close");

    vm.instance.call_depth = 0;
    assert!(vm.advance_map_iterator(7).expect("outer advance"));
    assert_eq!(
        vm.take_map_iterator_key(7).expect("outer key"),
        Value::string("outer")
    );
}

#[test]
#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
fn native_trace_cache_resets_when_program_changes() {
    let _guard = native_cache_test_lock()
        .lock()
        .expect("native cache test lock should succeed");
    jit::runtime::clear_native_trace_cache_for_tests();

    let source_one = r#"
        let mut i = 0;
        let mut sum = 0;
        while i < 8 {
            sum = sum + i;
            i = i + 1;
        }
        sum;
    "#;
    let source_two = r#"
        let mut k = 0;
        let mut total = 0;
        while k < 9 {
            total = total + k;
            k = k + 1;
        }
        total;
    "#;

    let compiled_one = crate::compile_source(source_one).expect("source one should compile");
    let compiled_two = crate::compile_source(source_two).expect("source two should compile");

    let mut vm_one = Vm::try_new(compiled_one.program).expect("test VM construction must not fail");
    vm_one.set_jit_config(jit::JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    let status_one = vm_one.run().expect("first vm should run");
    assert_eq!(status_one, VmStatus::Halted);
    let vm_one_trace_count = vm_one.jit_native_trace_count();
    assert!(
        vm_one_trace_count > 0,
        "first vm should produce native traces"
    );

    let (cache_program_after_one, cache_entries_after_one) =
        jit::runtime::native_trace_cache_snapshot_for_tests();
    assert_eq!(
        cache_program_after_one,
        Some(vm_one.engine.program_cache_key),
        "cache should be keyed to first program after first run"
    );
    assert_eq!(
        cache_entries_after_one, vm_one_trace_count,
        "cache entry count should match first program traces"
    );

    let mut vm_two = Vm::try_new(compiled_two.program).expect("test VM construction must not fail");
    vm_two.set_jit_config(jit::JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    assert_ne!(
        vm_one.engine.program_cache_key, vm_two.engine.program_cache_key,
        "test programs should have different cache keys"
    );
    let status_two = vm_two.run().expect("second vm should run");
    assert_eq!(status_two, VmStatus::Halted);
    let vm_two_trace_count = vm_two.jit_native_trace_count();
    assert!(
        vm_two_trace_count > 0,
        "second vm should produce native traces"
    );

    let (cache_program_after_two, cache_entries_after_two) =
        jit::runtime::native_trace_cache_snapshot_for_tests();
    assert_eq!(
        cache_program_after_two,
        Some(vm_two.engine.program_cache_key),
        "cache should switch to second program key"
    );
    assert_eq!(
        cache_entries_after_two, vm_two_trace_count,
        "cache should only contain traces from the active program"
    );
}

#[test]
#[cfg(any(
    all(
        target_arch = "x86_64",
        any(target_os = "windows", all(unix, not(target_os = "macos")))
    ),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
fn native_trace_cache_reuses_entries_for_same_program() {
    let _guard = native_cache_test_lock()
        .lock()
        .expect("native cache test lock should succeed");
    jit::runtime::clear_native_trace_cache_for_tests();

    let source = r#"
        let mut i = 0;
        let mut sum = 0;
        while i < 10 {
            sum = sum + i;
            i = i + 1;
        }
        sum;
    "#;
    let compiled = crate::compile_source(source).expect("source should compile");

    let mut vm_one =
        Vm::try_new(compiled.program.clone()).expect("test VM construction must not fail");
    vm_one.set_jit_config(jit::JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    let status_one = vm_one.run().expect("first vm should run");
    assert_eq!(status_one, VmStatus::Halted);
    let vm_one_trace_count = vm_one.jit_native_trace_count();
    assert!(
        vm_one_trace_count > 0,
        "first vm should produce native traces"
    );

    let (cache_program_after_one, cache_entries_after_one) =
        jit::runtime::native_trace_cache_snapshot_for_tests();
    assert_eq!(
        cache_program_after_one,
        Some(vm_one.engine.program_cache_key),
        "cache should be keyed to the first program"
    );
    assert_eq!(
        cache_entries_after_one, vm_one_trace_count,
        "cache entry count should match first vm traces"
    );

    let mut vm_two = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    vm_two.set_jit_config(jit::JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    assert_eq!(
        vm_two.engine.program_cache_key, vm_one.engine.program_cache_key,
        "same program should use identical cache key"
    );

    let status_two = vm_two.run().expect("second vm should run");
    assert_eq!(status_two, VmStatus::Halted);
    let vm_two_trace_count = vm_two.jit_native_trace_count();
    assert_eq!(
        vm_two_trace_count, vm_one_trace_count,
        "same program should compile same native trace count"
    );

    let (cache_program_after_two, cache_entries_after_two) =
        jit::runtime::native_trace_cache_snapshot_for_tests();
    assert_eq!(
        cache_program_after_two,
        Some(vm_two.engine.program_cache_key),
        "cache key should remain the same for identical program"
    );
    assert_eq!(
        cache_entries_after_two, cache_entries_after_one,
        "cache entries should be reused instead of duplicated"
    );
}

fn step_once(vm: &mut Vm) -> VmResult<ExecOutcome> {
    let opcode = vm.read_u8()?;
    vm.execute_interpreter_instruction(opcode, true)
}

fn assert_shared_heap_backing(lhs: &Value, rhs: &Value) {
    match (lhs, rhs) {
        (Value::String(lhs), Value::String(rhs)) => {
            assert!(Arc::ptr_eq(lhs, rhs), "expected shared string backing");
        }
        (Value::Array(lhs), Value::Array(rhs)) => {
            assert!(Arc::ptr_eq(lhs, rhs), "expected shared array backing");
        }
        (Value::Map(lhs), Value::Map(rhs)) => {
            assert!(Arc::ptr_eq(lhs, rhs), "expected shared map backing");
        }
        _ => panic!("expected matching heap values, got lhs={lhs:?} rhs={rhs:?}"),
    }
}

#[test]
fn interpreter_metrics_track_operand_hint_hits_for_typed_add() {
    let mut operand_types = HashMap::new();
    operand_types.insert(4usize, (ValueType::Int, ValueType::Int));
    let program = Program::new(
        vec![],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Ldloc as u8,
            1,
            OpCode::Add as u8,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(2)
    .with_type_map(TypeMap {
        strict_types: true,
        local_types: vec![ValueType::Int, ValueType::Int],
        local_schemas: vec![None, None],
        callable_slots: vec![false, false],
        optional_slots: vec![false, false],
        operand_types,
    });
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.set_local(0, Value::Int(7))
        .expect("setting first local should succeed");
    vm.set_local(1, Value::Int(5))
        .expect("setting second local should succeed");

    let status = vm.run().expect("typed add program should run");

    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(12)]);
    let metrics = vm.interpreter_metrics_snapshot();
    assert_eq!(metrics.operand_hint_hit_count, 1);
    assert_eq!(metrics.operand_hint_miss_count, 0);
}

#[test]
fn interpreter_uses_typed_builtin_fast_path_for_slice_calls() {
    let [call_lo, call_hi] = BuiltinFunction::Slice.call_index().to_le_bytes();
    let mut operand_types = HashMap::new();
    operand_types.insert(15usize, (ValueType::String, ValueType::Int));
    let program = Program::new(
        vec![Value::string("abcd"), Value::Int(1), Value::Int(2)],
        vec![
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Ldc as u8,
            1,
            0,
            0,
            0,
            OpCode::Ldc as u8,
            2,
            0,
            0,
            0,
            OpCode::Call as u8,
            call_lo,
            call_hi,
            3,
            OpCode::Ret as u8,
        ],
    )
    .with_type_map(TypeMap {
        strict_types: true,
        local_types: Vec::new(),
        local_schemas: Vec::new(),
        callable_slots: Vec::new(),
        optional_slots: Vec::new(),
        operand_types,
    });
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");

    let status = vm.run().expect("typed slice builtin should run");

    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::string("bc")]);
    let metrics = vm.interpreter_metrics_snapshot();
    assert_eq!(metrics.typed_builtin_fast_path_count, 1);
    assert_eq!(metrics.projection_fast_path_count, 0);
    assert_eq!(metrics.generic_builtin_call_count, 0);
}

#[test]
fn interpreter_superinstructions_use_local_type_hints() {
    let program = Program::new(
        vec![Value::Int(1)],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Add as u8,
            OpCode::Stloc as u8,
            0,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(1)
    .with_type_map(TypeMap {
        strict_types: true,
        local_types: vec![ValueType::Int],
        local_schemas: vec![None],
        callable_slots: vec![false],
        optional_slots: vec![false],
        operand_types: HashMap::new(),
    });
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.set_local(0, Value::Int(9))
        .expect("setting local should succeed");

    let outcome = step_once(&mut vm).expect("ldloc should fuse scalar sequence");

    assert!(matches!(outcome, ExecOutcome::Continue));
    assert_eq!(vm.instance.locals[0], Value::Int(10));
    let metrics = vm.interpreter_metrics_snapshot();
    assert_eq!(metrics.scalar_superinstruction_count, 1);
    assert!(
        metrics.local_type_hint_hit_count >= 1,
        "expected local type hints to seed superinstruction execution"
    );
}

#[test]
fn interpreter_ldc_shares_string_constant_backing() {
    let program = Program::new(
        vec![Value::string("shared")],
        vec![OpCode::Ldc as u8, 0, 0, 0, 0, OpCode::Ret as u8],
    );
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");

    let outcome = step_once(&mut vm).expect("ldc should execute");
    assert!(matches!(outcome, ExecOutcome::Continue));
    let constant = vm
        .program
        .constants
        .first()
        .expect("program should keep a constant");
    assert_shared_heap_backing(constant, &vm.stack()[0]);
}

#[test]
fn interpreter_dup_shares_array_backing() {
    let program = Program::new(vec![], vec![OpCode::Dup as u8, OpCode::Ret as u8]);
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.instance
        .stack
        .push(Value::array(vec![Value::Int(1), Value::Int(2)]));

    let outcome = step_once(&mut vm).expect("dup should execute");
    assert!(matches!(outcome, ExecOutcome::Continue));
    assert_eq!(vm.stack().len(), 2);
    assert_shared_heap_backing(&vm.stack()[0], &vm.stack()[1]);
}

#[test]
fn shared_string_survives_local_overwrite_after_copy_like_read() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let program = Program::new(
        vec![Value::Null],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Dup as u8,
            OpCode::Stloc as u8,
            0,
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Stloc as u8,
            0,
            OpCode::Call as u8,
            call_lo,
            call_hi,
            1,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(1);
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.set_local(0, Value::string("alive"))
        .expect("setting local should succeed");

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.locals()[0], Value::Null);
    assert_eq!(vm.stack(), &[Value::Int(5)]);
}

#[test]
fn shared_array_survives_local_overwrite_after_copy_like_read() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let program = Program::new(
        vec![Value::Null],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Dup as u8,
            OpCode::Stloc as u8,
            0,
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Stloc as u8,
            0,
            OpCode::Call as u8,
            call_lo,
            call_hi,
            1,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(1);
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.set_local(0, Value::array(vec![Value::Int(1), Value::Int(2)]))
        .expect("setting local should succeed");

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.locals()[0], Value::Null);
    assert_eq!(vm.stack(), &[Value::Int(2)]);
}

#[test]
fn shared_map_survives_local_overwrite_after_copy_like_read() {
    let [call_lo, call_hi] = BuiltinFunction::Count.call_index().to_le_bytes();
    let program = Program::new(
        vec![Value::Null],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Dup as u8,
            OpCode::Stloc as u8,
            0,
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Stloc as u8,
            0,
            OpCode::Call as u8,
            call_lo,
            call_hi,
            1,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(1);
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.set_local(0, Value::map(vec![(Value::string("k"), Value::Int(9))]))
        .expect("setting local should succeed");

    let status = vm.run().expect("vm should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.locals()[0], Value::Null);
    assert_eq!(vm.stack(), &[Value::Int(1)]);
}

#[test]
fn interpreter_ldloc_preserves_local_slot() {
    let program =
        Program::new(vec![], vec![OpCode::Ldloc as u8, 0, OpCode::Ret as u8]).with_local_count(1);
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    let map_value = Value::map(vec![(Value::string("k"), Value::Int(9))]);
    vm.set_local(0, map_value.clone())
        .expect("setting local should succeed");

    let outcome = step_once(&mut vm).expect("ldloc should execute");
    assert!(matches!(outcome, ExecOutcome::Continue));
    assert_eq!(vm.instance.ip, 2);
    assert_eq!(
        vm.instance.locals[0], map_value,
        "ldloc should leave local intact"
    );
    assert_eq!(
        vm.stack(),
        &[map_value],
        "stack should receive copied value"
    );
    assert_shared_heap_backing(&vm.instance.locals[0], &vm.stack()[0]);
    assert_eq!(vm.drop_contract_event_count(), 0);
}

#[test]
fn interpreter_explicit_move_sequence_clears_local_slot() {
    let program = Program::new(
        vec![Value::Null],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Stloc as u8,
            0,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(1);
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    let map_value = Value::map(vec![(Value::string("k"), Value::Int(9))]);
    vm.set_local(0, map_value.clone())
        .expect("setting local should succeed");

    let ldloc = step_once(&mut vm).expect("ldloc should execute");
    assert!(matches!(ldloc, ExecOutcome::Continue));
    assert_eq!(vm.instance.locals[0], map_value);
    assert_eq!(vm.stack(), std::slice::from_ref(&map_value));
    assert_shared_heap_backing(&vm.instance.locals[0], &vm.stack()[0]);

    let ldc = step_once(&mut vm).expect("ldc should execute");
    assert!(matches!(ldc, ExecOutcome::Continue));
    assert_eq!(vm.stack(), &[map_value.clone(), Value::Null]);

    let stloc = step_once(&mut vm).expect("stloc should execute");
    assert!(matches!(stloc, ExecOutcome::Continue));
    assert_eq!(vm.instance.ip, 9);
    assert_eq!(vm.instance.locals[0], Value::Null);
    assert_eq!(vm.stack(), &[map_value]);
}

#[test]
fn interpreter_fuses_ldloc_ldc_add_stloc_without_touching_stack() {
    let program = Program::new(
        vec![Value::Int(1)],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Add as u8,
            OpCode::Stloc as u8,
            1,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(2);
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.set_local(0, Value::Int(41))
        .expect("setting local should succeed");

    let outcome = step_once(&mut vm).expect("fused sequence should execute");
    assert!(matches!(outcome, ExecOutcome::Continue));
    assert_eq!(vm.instance.ip, 10, "fusion should consume ldc/add/stloc");
    assert_eq!(vm.instance.locals[0], Value::Int(41));
    assert_eq!(vm.instance.locals[1], Value::Int(42));
    assert!(
        vm.stack().is_empty(),
        "fusion should avoid transient stack traffic"
    );
}

#[test]
fn interpreter_fuses_ldloc_ldc_compare_brfalse() {
    let program = Program::new(
        vec![Value::Int(10), Value::Int(1)],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Clt as u8,
            OpCode::Brfalse as u8,
            15,
            0,
            0,
            0,
            OpCode::Ldc as u8,
            1,
            0,
            0,
            0,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(1);
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.set_local(0, Value::Int(42))
        .expect("setting local should succeed");

    let outcome = step_once(&mut vm).expect("fused compare should execute");
    assert!(matches!(outcome, ExecOutcome::Continue));
    assert_eq!(
        vm.instance.ip, 15,
        "fusion should jump directly to branch target"
    );
    assert!(
        vm.stack().is_empty(),
        "fusion should avoid bool stack traffic"
    );
}

#[test]
fn interpreter_fuses_generic_scalar_update_chain() {
    let program = Program::new(
        vec![Value::Int(3), Value::Int(7)],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Ldloc as u8,
            1,
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Mul as u8,
            OpCode::Add as u8,
            OpCode::Ldc as u8,
            1,
            0,
            0,
            0,
            OpCode::Add as u8,
            OpCode::Stloc as u8,
            0,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(2);
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.set_local(0, Value::Int(10))
        .expect("setting local should succeed");
    vm.set_local(1, Value::Int(4))
        .expect("setting local should succeed");

    let outcome = step_once(&mut vm).expect("generic chain should fuse");
    assert!(matches!(outcome, ExecOutcome::Continue));
    assert_eq!(vm.instance.ip, 19);
    assert_eq!(vm.instance.locals[0], Value::Int(29));
    assert_eq!(vm.instance.locals[1], Value::Int(4));
    assert!(vm.stack().is_empty());
}

#[test]
fn interpreter_fuses_float_scalar_sequences() {
    let program = Program::new(
        vec![Value::Float(1.5), Value::Float(2.0)],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Add as u8,
            OpCode::Stloc as u8,
            0,
            OpCode::Ldloc as u8,
            0,
            OpCode::Ldc as u8,
            1,
            0,
            0,
            0,
            OpCode::Cgt as u8,
            OpCode::Brfalse as u8,
            24,
            0,
            0,
            0,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(1);
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.set_local(0, Value::Float(1.0))
        .expect("setting local should succeed");

    let first = step_once(&mut vm).expect("float update should fuse");
    assert!(matches!(first, ExecOutcome::Continue));
    assert_eq!(vm.instance.ip, 10);
    assert_eq!(vm.instance.locals[0], Value::Float(2.5));
    assert!(vm.stack().is_empty());

    let second = step_once(&mut vm).expect("float compare should fuse");
    assert!(matches!(second, ExecOutcome::Continue));
    assert_eq!(vm.instance.ip, 23);
    assert!(vm.stack().is_empty());
}

#[test]
fn interpreter_does_not_fuse_ldloc_sequences_when_fuel_is_enabled() {
    let program = Program::new(
        vec![Value::Int(1)],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Ldc as u8,
            0,
            0,
            0,
            0,
            OpCode::Add as u8,
            OpCode::Stloc as u8,
            0,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(1);
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.set_local(0, Value::Int(41))
        .expect("setting local should succeed");
    vm.set_fuel(32);

    let opcode = vm.read_u8().expect("ldloc opcode should decode");
    let outcome = vm
        .execute_interpreter_instruction(opcode, false)
        .expect("ldloc should execute without fusion");
    assert!(matches!(outcome, ExecOutcome::Continue));
    assert_eq!(
        vm.instance.ip, 2,
        "ldloc should advance only past its operand"
    );
    assert_eq!(vm.stack(), &[Value::Int(41)]);
    assert_eq!(vm.instance.locals[0], Value::Int(41));
}

#[test]
fn interpreter_copy_like_ldloc_dup_stloc_shares_map_backing_with_fuel() {
    let program = Program::new(
        vec![],
        vec![
            OpCode::Ldloc as u8,
            0,
            OpCode::Dup as u8,
            OpCode::Stloc as u8,
            0,
            OpCode::Ret as u8,
        ],
    )
    .with_local_count(1);
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.set_local(0, Value::map(vec![(Value::string("k"), Value::Int(9))]))
        .expect("setting local should succeed");
    vm.set_fuel(32);

    let _ = step_once(&mut vm).expect("ldloc should execute");
    let _ = step_once(&mut vm).expect("dup should execute");
    let _ = step_once(&mut vm).expect("stloc should execute");

    assert_eq!(vm.stack().len(), 1);
    assert_shared_heap_backing(&vm.instance.locals[0], &vm.stack()[0]);
}

#[test]
fn interpreter_fuses_call_ret_without_fuel() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let program = Program::new(
        vec![],
        vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
    );
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.instance.stack.push(Value::string("tail"));

    let outcome = step_once(&mut vm).expect("call should execute");
    assert!(matches!(outcome, ExecOutcome::Halted));
    assert_eq!(
        vm.instance.ip, 5,
        "tail-call fusion should consume trailing ret"
    );
    assert_eq!(vm.stack(), &[Value::Int(4)]);
}

#[test]
fn interpreter_fuses_call_ret_when_fuel_enabled_if_tail_tick_available() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let program = Program::new(
        vec![],
        vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
    );
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.set_fuel(1);
    vm.instance.stack.push(Value::string("tail"));

    // `step_once` bypasses the outer run-loop pre-tick, so this fuel only covers fused `ret`.
    let call = step_once(&mut vm).expect("call should execute");
    assert!(matches!(call, ExecOutcome::Halted));
    assert_eq!(
        vm.instance.ip, 5,
        "tail-call fusion should consume trailing ret"
    );
    assert_eq!(vm.stack(), &[Value::Int(4)]);
    assert_eq!(vm.get_fuel(), Some(0));
}

#[test]
fn interpreter_call_ret_fusion_preserves_ip_when_tail_tick_exhausted() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let program = Program::new(
        vec![],
        vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
    );
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.set_fuel(0);
    vm.instance.stack.push(Value::string("tail"));

    let err = match step_once(&mut vm) {
        Ok(_) => panic!("tail tick should fail with out-of-fuel"),
        Err(err) => err,
    };
    assert!(matches!(err, VmError::OutOfFuel { .. }));
    assert_eq!(
        vm.instance.ip, 4,
        "ret must remain pending when tail tick cannot be charged"
    );
    assert_eq!(vm.stack(), &[Value::Int(4)]);
}

#[test]
fn interpreter_call_ret_fusion_preserves_ip_when_epoch_deadline_is_reached() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let program = Program::new(
        vec![],
        vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
    );
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.set_epoch_deadline(0)
        .expect("setting epoch deadline should succeed");
    vm.instance.stack.push(Value::string("tail"));

    let err = match step_once(&mut vm) {
        Ok(_) => panic!("tail tick should fail with epoch deadline reached"),
        Err(err) => err,
    };
    assert!(matches!(err, VmError::EpochDeadlineReached { .. }));
    assert_eq!(
        vm.instance.ip, 4,
        "ret must remain pending when the epoch check trips during fused tail execution"
    );
    assert_eq!(vm.stack(), &[Value::Int(4)]);
}

#[test]
fn run_consumes_two_ticks_for_call_ret_when_fuel_enabled() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let program = Program::new(
        vec![],
        vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
    );
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.set_fuel(2);
    vm.instance.stack.push(Value::string("tail"));

    let status = vm.run().expect("run should complete");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.instance.ip, 5);
    assert_eq!(vm.stack(), &[Value::Int(4)]);
    assert_eq!(
        vm.get_fuel(),
        Some(0),
        "call+ret should spend two ticks with fuel metering enabled"
    );
}

#[test]
fn run_yields_before_ret_in_call_ret_sequence_when_out_of_fuel() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let program = Program::new(
        vec![],
        vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
    );
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.set_fuel(1);
    vm.instance.stack.push(Value::string("tail"));

    let status = vm.run().expect("first run should yield");
    assert_eq!(status, VmStatus::Yielded);
    assert_eq!(
        vm.instance.ip, 4,
        "fuel exhaustion should happen before trailing ret"
    );
    assert_eq!(vm.stack(), &[Value::Int(4)]);
    assert_eq!(vm.get_fuel(), Some(0));

    vm.add_fuel(1).expect("recharging fuel should succeed");
    let resumed = vm.resume().expect("resume should execute trailing ret");
    assert_eq!(resumed, VmStatus::Halted);
    assert_eq!(vm.instance.ip, 5);
    assert_eq!(vm.stack(), &[Value::Int(4)]);
}

#[test]
fn run_yields_before_ret_in_call_ret_sequence_when_epoch_deadline_is_reached() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let program = Program::new(
        vec![],
        vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
    );
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.set_epoch_check_interval(2)
        .expect("epoch interval update should succeed");
    vm.set_epoch_deadline(1)
        .expect("setting epoch deadline should succeed");
    assert_eq!(vm.increment_epoch(), 1);
    vm.instance.stack.push(Value::string("tail"));

    let status = vm.run().expect("first run should yield");
    assert_eq!(status, VmStatus::Yielded);
    assert_eq!(
        vm.instance.ip, 4,
        "epoch interruption should happen before trailing ret"
    );
    assert_eq!(vm.last_yield_reason(), Some(VmYieldReason::Epoch));
    assert_eq!(vm.stack(), &[Value::Int(4)]);

    let resumed = vm
        .resume()
        .expect("resume should auto re-arm the epoch deadline and execute trailing ret");
    assert_eq!(resumed, VmStatus::Halted);
    assert_eq!(vm.instance.ip, 5);
    assert_eq!(vm.stack(), &[Value::Int(4)]);
}

#[test]
fn dropping_pre_cancelled_invocation_consumes_cancellation_at_the_boundary() {
    // Dropping an invocation with a pending typed cancellation retires that
    // invocation without manufacturing an unobservable terminal item. The VM
    // cancellation root is refreshed for immediate reuse.
    let compiled = crate::compile_source(
        r#"
        pub fn run() -> int {
            42;
        }
        "#,
    )
    .expect("invocation source should compile");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    assert_eq!(vm.run().expect("root frame should halt"), VmStatus::Halted);

    vm.run_ctx
        .cancel(CancellationReason::Requested)
        .expect("pre-cancellation should be accepted");
    let callable = vm
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    {
        let _invocation = vm
            .start_invocation(callable.clone(), vec![])
            .expect("invocation should start");
    }
    assert!(
        vm.run_ctx.cancellation.reason().is_none(),
        "dropping the invocation must consume its pending cancellation"
    );

    let mut replacement = vm
        .start_invocation(callable, vec![])
        .expect("the vm should be reusable after the dropped invocation");
    assert!(matches!(
        replacement.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(Some(Ok(InvocationItem::Complete(Value::Int(42)))))
    ));
}

#[test]
fn pre_cancelled_invocation_delivers_one_typed_error_then_fused_end() {
    // Functional contract of the pre-cancelled path: exactly one typed
    // Cancelled item, a fused end, and the cancellation consumed at the
    // invocation boundary (a later invocation runs normally).
    let compiled = crate::compile_source(
        r#"
        pub fn run() -> int {
            42;
        }
        "#,
    )
    .expect("invocation source should compile");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    assert_eq!(vm.run().expect("root frame should halt"), VmStatus::Halted);

    vm.run_ctx
        .cancel(CancellationReason::Requested)
        .expect("pre-cancellation should be accepted");
    let callable = vm
        .resolve_exported_callable("run")
        .expect("exported run callable should resolve");
    {
        let mut invocation = vm
            .start_invocation(callable.clone(), vec![])
            .expect("invocation should start");

        match invocation.poll_next().expect("poll should succeed") {
            InvocationPoll::Ready(Some(Err(InvocationError::Cancelled(
                CancellationReason::Requested,
            )))) => {}
            other => panic!("expected a typed cancellation item, got {other:?}"),
        }
        assert!(matches!(
            invocation.poll_next().expect("poll should succeed"),
            InvocationPoll::Ready(None)
        ));
    }

    // A new invocation on the same VM must run to completion instead of
    // being cancelled on arrival.
    let mut second = vm
        .start_invocation(callable, vec![])
        .expect("a new invocation may start after fusion");
    match second.poll_next().expect("poll should succeed") {
        InvocationPoll::Ready(Some(Ok(InvocationItem::Complete(Value::Int(42))))) => {}
        other => panic!("the second invocation must complete normally, got {other:?}"),
    }
    assert!(matches!(
        second.poll_next().expect("poll should succeed"),
        InvocationPoll::Ready(None)
    ));
}

#[test]
fn call_ret_fusion_pattern_requires_immediate_ret() {
    let [call_lo, call_hi] = BuiltinFunction::Len.call_index().to_le_bytes();
    let with_ret = Program::new(
        vec![],
        vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Ret as u8],
    );
    let mut vm_with_ret = Vm::try_new(with_ret).expect("test VM construction must not fail");
    vm_with_ret.instance.ip = 4;
    assert!(vm_with_ret.can_fuse_call_ret_pattern());

    let wrong_next = Program::new(
        vec![],
        vec![OpCode::Call as u8, call_lo, call_hi, 1, OpCode::Nop as u8],
    );
    let mut vm_wrong_next = Vm::try_new(wrong_next).expect("test VM construction must not fail");
    vm_wrong_next.instance.ip = 4;
    assert!(!vm_wrong_next.can_fuse_call_ret_pattern());

    let no_next = Program::new(vec![], vec![OpCode::Call as u8, call_lo, call_hi, 1]);
    let mut vm_no_next = Vm::try_new(no_next).expect("test VM construction must not fail");
    vm_no_next.instance.ip = 4;
    assert!(!vm_no_next.can_fuse_call_ret_pattern());
}

#[test]
fn program_cache_key_distinguishes_call_script_from_call_value() {
    // A direct-only call lowers to `CallScript`; the same call through a
    // materialized callable lowers to `CallValue`. The static cache identity
    // must treat the two programs as different even when their metadata
    // otherwise matches, because the native call boundary differs.
    let direct = crate::compile_source("fn add2(value: int) -> int { value + 2 } add2(40);")
        .expect("direct call source should compile");
    let materialized =
        crate::compile_source("fn add2(value: int) -> int { value + 2 } let f = add2; f(40);")
            .expect("materialized call source should compile");

    let mut direct_vm = Vm::try_new(direct.program).expect("test VM construction must not fail");
    let mut materialized_vm =
        Vm::try_new(materialized.program).expect("test VM construction must not fail");
    let direct_key = direct_vm.ensure_program_cache_key();
    let materialized_key = materialized_vm.ensure_program_cache_key();
    assert_ne!(
        direct_key, materialized_key,
        "CallScript and CallValue programs must not share cache identity"
    );

    // The same direct program reproduces the same key across VMs.
    let direct_repeat = crate::compile_source("fn add2(value: int) -> int { value + 2 } add2(40);")
        .expect("direct call source should compile");
    let mut repeat_vm =
        Vm::try_new(direct_repeat.program).expect("test VM construction must not fail");
    assert_eq!(
        repeat_vm.ensure_program_cache_key(),
        direct_key,
        "identical programs must share cache identity"
    );
}

#[test]
fn native_callable_abi_version_covers_direct_script_calls() {
    // `CallScript` adds a new native boundary helper and exit contract, and
    // the JIT inline ownership bridge adds the root-callable materialization
    // helper; the native callable ABI revision must reflect both so every
    // directly coupled program/native cache is invalidated exactly once.
    assert_eq!(
        super::native::NATIVE_CALLABLE_ABI_VERSION,
        7,
        "native callable ABI revision must cover direct script call and root-callable materialization semantics"
    );
    let direct = crate::compile_source("fn add2(value: int) -> int { value + 2 } add2(40);")
        .expect("direct call source should compile");
    let mut vm = Vm::try_new(direct.program).expect("test VM construction must not fail");
    let key = vm.ensure_program_cache_key();
    assert_ne!(key, 0, "cache key must be non-trivial");
}

#[test]
fn try_new_returns_typed_exhaustion_error_and_never_panics() {
    use crate::vm::resource::table::test_seam::ScopedArenaSource;
    // Fresh counter per test: the first construction consumes the max handout,
    // the second is the first call after the max.
    static COUNTER: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(crate::vm::resource::handle::MAX_HANDLE_ARENA_ID);
    let _source = ScopedArenaSource::install(&COUNTER);

    // The first construction consumes the max handout.
    let _first = Vm::try_new(Program::new(Vec::new(), vec![OpCode::Ret as u8]))
        .expect("the last arena id must construct a vm");
    // The second is the first call after the max handout.
    let error = match Vm::try_new(Program::new(Vec::new(), vec![OpCode::Ret as u8])) {
        Ok(_) => panic!("arena space must be exhausted"),
        Err(error) => error,
    };
    let resource = error.resource_error().expect("typed resource error");
    assert_eq!(
        resource.code(),
        ResourceErrorCode::ResourceTableArenaExhausted,
        "typed arena-exhaustion code must survive ResourceTable -> ExecutionScope -> HostRuntime -> Vm::try_new"
    );
}

#[test]
fn try_new_shared_with_jit_config_propagates_exhaustion_typed() {
    use crate::vm::resource::table::test_seam::ScopedArenaSource;
    static COUNTER: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(crate::vm::resource::handle::MAX_HANDLE_ARENA_ID);
    let _source = ScopedArenaSource::install(&COUNTER);

    let _first = Vm::try_new_shared_with_jit_config(
        Arc::new(Program::new(Vec::new(), vec![OpCode::Ret as u8])),
        crate::vm::jit::JitConfig::default(),
    )
    .expect("last arena id must construct");
    let error = match Vm::try_new_shared_with_jit_config(
        Arc::new(Program::new(Vec::new(), vec![OpCode::Ret as u8])),
        crate::vm::jit::JitConfig::default(),
    ) {
        Ok(_) => panic!("arena space must be exhausted"),
        Err(error) => error,
    };
    assert_eq!(
        error.resource_error().expect("typed").code(),
        ResourceErrorCode::ResourceTableArenaExhausted
    );
}

/// Source-contract guard: the shipped VM construction path is fallible.
///
/// [`Vm::try_new`], [`Vm::try_new_shared`] and `CompiledProgram::into_vm`
/// must return a `Result` so downstream production callers can propagate the
/// typed arena-exhaustion error instead of panicking. The type-checking calls
/// below fail to compile if any future change regresses those constructors or
/// `into_vm` back to an infallible `-> Vm`.
///
/// The former infallible `Vm::new` / `Vm::new_with_jit_config` /
/// `Vm::new_shared` public shims are intentionally absent: they are removed
/// from the public API, so no downstream-callable production path can panic on
/// arena exhaustion. Re-adding them must be treated as a breaking regression.
#[test]
fn shipped_construction_paths_are_fallible() {
    fn needs_vm_result(_value: VmResult<Vm>) {}
    fn needs_unit_result(_value: VmResult<()>) {}

    let program = Program::new(Vec::new(), vec![OpCode::Ret as u8]);
    needs_vm_result(Vm::try_new(program.clone()));
    needs_vm_result(Vm::try_new_shared(Arc::new(program.clone())));
    needs_vm_result(Vm::try_new_with_jit_config(
        program.clone(),
        crate::vm::jit::JitConfig::default(),
    ));
    needs_unit_result(
        crate::compiler::CompiledProgram {
            program: program.clone(),
            locals: 0,
            functions: Vec::new(),
            callable_use_facts: Vec::new(),
        }
        .into_vm()
        .map(|_| ()),
    );

    // Each fallible path, when it does succeed, yields a fully operational VM
    // (a plain infallible shim that only wrapped the same body would silence
    // the typed error but not change behavior here).
    let mut vm = Vm::try_new(program).expect("construction should succeed");
    vm.reset_for_reuse();
    assert!(vm.is_reusable());
}

#[test]
fn reset_at_arena_exhaustion_poisons_and_preserves_old_scope() {
    use crate::vm::resource::table::test_seam::ScopedArenaSource;
    use std::task::Wake;

    struct NoopWake;
    impl Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
    }

    let mut vm = Vm::try_new(Program::new(Vec::new(), vec![OpCode::Ret as u8]))
        .expect("test VM construction must not fail");
    assert_eq!(vm.run().expect("run"), VmStatus::Halted);

    // Exhaust the arena for the recycle step. The counter is set past the max
    // handout so the recycle's first (and only) allocation already fails. The
    // scope close itself never allocates an arena id, so driving the reset to
    // completion quiesces cleanup and then fails atomically at the recycle.
    static COUNTER: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(crate::vm::resource::handle::MAX_HANDLE_ARENA_ID + 1);
    let _source = ScopedArenaSource::install(&COUNTER);

    vm.begin_reset_for_reuse(ResourceCloseReason::VmReset, None)
        .expect("begin reset");
    let waker = Arc::new(NoopWake).into();
    let mut cx = std::task::Context::from_waker(&waker);
    let mut drive_count = 0;
    let result = loop {
        drive_count += 1;
        assert!(
            drive_count < 64,
            "reset must reach a terminal state promptly"
        );
        match vm.poll_reset_for_reuse(&mut cx, std::time::Instant::now()) {
            std::task::Poll::Pending => continue,
            std::task::Poll::Ready(result) => break result,
        }
    };

    let Err(error) = result else {
        panic!("recycle at arena exhaustion must poison the vm");
    };
    match error {
        VmError::Reset(VmResetError::ScopeRecycle(
            super::execution_scope::ExecutionScopeError::ArenaExhausted(resource),
        )) => {
            assert_eq!(
                resource.code(),
                ResourceErrorCode::ResourceTableArenaExhausted,
                "typed arena-exhaustion code must survive the reset/recycle path"
            );
        }
        other => panic!("expected ScopeRecycle(ArenaExhausted), got {other:?}"),
    }

    // The VM is permanently poisoned: not reusable, error preserved, old scope
    // kept for diagnostics (no partial reset, no malformed scope install).
    assert_eq!(vm.reset_state(), VmResetState::Poisoned);
    assert!(!vm.is_reusable());
    assert!(matches!(
        vm.reset_error(),
        Some(VmResetError::ScopeRecycle(
            super::execution_scope::ExecutionScopeError::ArenaExhausted(_)
        ))
    ));
    // The old scope remains installed and quiescent (cleanup finished).
    assert!(vm.host.execution_scope_is_quiescent());
    assert_eq!(vm.host.execution_scope_resource_count(), 0);
    assert_eq!(vm.host.execution_scope_operation_count(), 0);
    // A further reset attempt is rejected typed.
    let rejected = vm.begin_reset_for_reuse(ResourceCloseReason::VmReset, None);
    assert!(matches!(
        rejected,
        Err(VmError::Reset(VmResetError::AlreadyPoisoned { .. }))
    ));
    // Drop safety: dropping the poisoned VM must not panic.
}

#[test]
fn reset_recycle_succeeds_when_arena_is_available_again() {
    use crate::vm::resource::table::test_seam::ScopedArenaSource;
    use std::task::Wake;

    struct NoopWake;
    impl Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
    }

    let mut vm = Vm::try_new(Program::new(Vec::new(), vec![OpCode::Ret as u8]))
        .expect("test VM construction must not fail");
    assert_eq!(vm.run().expect("run"), VmStatus::Halted);
    vm.begin_reset_for_reuse(ResourceCloseReason::VmReset, None)
        .expect("begin reset");

    // A scoped exhaustion window that is released before the recycle step.
    {
        static COUNTER: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(crate::vm::resource::handle::MAX_HANDLE_ARENA_ID);
        let _source = ScopedArenaSource::install(&COUNTER);
    }

    let waker = Arc::new(NoopWake).into();
    let mut cx = std::task::Context::from_waker(&waker);
    for _ in 0..64 {
        match vm.poll_reset_for_reuse(&mut cx, std::time::Instant::now()) {
            std::task::Poll::Pending => continue,
            std::task::Poll::Ready(result) => {
                result.expect("reset must complete once the arena is available");
                break;
            }
        }
    }
    assert_eq!(vm.reset_state(), VmResetState::Ready);
    assert!(vm.is_reusable());
}
