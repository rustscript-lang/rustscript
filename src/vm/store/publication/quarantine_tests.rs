use super::*;
use crate::HostResourceType;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Debug, Default)]
struct CloseState {
    begins: AtomicUsize,
    polls: AtomicUsize,
    drops: AtomicUsize,
    ready: AtomicBool,
    fail: AtomicBool,
}
#[derive(Debug)]
struct Item(Arc<CloseState>);
impl Drop for Item {
    fn drop(&mut self) {
        self.0.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl crate::resource::HostResource for Item {
    fn resource_type_key() -> Option<crate::ResourceTypeKey> {
        Some(crate::ResourceTypeKey::new(Self::KEY).unwrap())
    }
    fn begin_close(
        &mut self,
        _: crate::ResourceCloseReason,
    ) -> crate::resource::ResourceResult<crate::resource::CloseProgress> {
        self.0.begins.fetch_add(1, Ordering::SeqCst);
        if self.0.fail.load(Ordering::SeqCst) {
            return Err(crate::resource::ResourceError::new(
                crate::resource::ResourceErrorCode::ResourceCleanupFailed,
                "sdk_quarantine_test",
                "intentional close failure",
            ));
        }
        Ok(if self.0.ready.load(Ordering::SeqCst) {
            crate::resource::CloseProgress::Ready
        } else {
            crate::resource::CloseProgress::Pending
        })
    }
    fn poll_close(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<crate::resource::ResourceResult<()>> {
        self.0.polls.fetch_add(1, Ordering::SeqCst);
        if self.0.ready.load(Ordering::SeqCst) {
            std::task::Poll::Ready(Ok(()))
        } else {
            std::task::Poll::Pending
        }
    }
}
impl crate::HostResourceType for Item {
    const KEY: &'static str = "sdk.quarantine_item";
    const DESCRIPTION: &'static str = "quarantine lifecycle fixture";
}
fn constant_store(success: bool) -> crate::Store {
    let source = if success {
        "pub fn invoke(value: resource<sdk.quarantine_item>) -> resource<sdk.quarantine_item> { value } pub fn ordinary(value: int) -> int { value + 1 }"
    } else {
        "pub fn invoke(value: resource<sdk.quarantine_item>) -> resource<sdk.quarantine_item> { let n = 1 / 0; value } pub fn ordinary(value: int) -> int { value + 1 }"
    };
    let compiled = crate::compile_source_for_repl(source).unwrap();
    let mut program = compiled.program.with_local_count(compiled.locals);
    let mut deep = Value::Null;
    for _ in 0..MAX_DEPTH + 1 {
        deep = Value::array(vec![deep]);
    }
    program.constants.push(deep);
    let mut store = crate::Store::from_vm(Vm::new(program));
    store.run().unwrap();
    store
}
fn invoke(
    store: &mut crate::Store,
    state: &Arc<CloseState>,
) -> VmResult<crate::ResourceOwned<Item>> {
    store
        .vm_script_callback_by_name::<(crate::ResourceOwned<Item>,), crate::ResourceOwned<Item>>(
            "invoke",
        )?
        .call_with_vm(store, (crate::ResourceOwned::new(Item(state.clone())),))
}
#[test]
fn callback_publication_success_projection_refusal_preserves_host_return_and_blocks_public_run() {
    let mut store = constant_store(true);
    let queued = store
        .script_callback_by_name::<(i64,), i64>("ordinary")
        .unwrap();
    store
        .enqueue_callback(queued.prepare((7,)).unwrap())
        .unwrap();
    let state = Arc::new(CloseState::default());
    let error = invoke(&mut store, &state).unwrap_err();
    assert!(
        error.to_string().contains("ownership unresolved"),
        "{error}"
    );
    assert!(store.vm.instance.execution_frames.is_empty());
    let returned = store
        .vm
        .instance
        .host_return
        .clone()
        .expect("actual return must stay rooted");
    assert_eq!(store.vm.host_context().resource_count(), 1);
    assert_eq!(state.begins.load(Ordering::SeqCst), 0);
    let run = store.run();
    let direct_run = store.vm_mut().run();
    let second = invoke(&mut store, &state);
    eprintln!(
        "callback_projection_success unit_constant_fixture=true run={run:?} direct_run={direct_run:?} second={second:?} live={} host_return_preserved={}",
        store.vm.host_context().resource_count(),
        store.vm.instance.host_return.as_ref() == Some(&returned)
    );
    assert!(
        run.is_err(),
        "Store::run must reject unresolved successful return"
    );
    assert!(direct_run.is_err());
    assert!(second.is_err());
    assert_eq!(store.vm.instance.host_return.as_ref(), Some(&returned));
    assert!(store.resume().is_err());
    assert!(store.vm_mut().drain_callable_queue().is_err());
    assert_eq!(store.vm().queued_callable_count(), 1);
    assert!(queued.call(&mut store, (7,)).is_err());
    assert!(queued.start(&mut store, (7,)).is_err());
    assert!(
        store.vm_mut().take_callable_result().is_none(),
        "public result extraction must not drain an unclassified return"
    );
    assert_eq!(store.vm.instance.host_return.as_ref(), Some(&returned));
    assert_eq!(store.vm.host_context().resource_count(), 1);
    state.ready.store(true, Ordering::SeqCst);
    store.vm_mut().shutdown();
    assert_eq!(store.vm.host_context().resource_count(), 0);
    assert_eq!(state.begins.load(Ordering::SeqCst), 1);
    assert!(store.run().is_err(), "shutdown remains terminal");
}
#[test]
fn callback_publication_source_created_depth_limit_blocks_public_continuation() {
    let source = "pub fn invoke(value: resource<sdk.quarantine_item>) -> resource<sdk.quarantine_item> { let mut graph: array = []; let mut n = 0; while n < 65 { graph = [graph]; n = n + 1; } let bad = 1 / 0; graph; value }";
    let compiled = crate::compile_source_for_repl(source).unwrap();
    let mut store =
        crate::Store::from_vm(Vm::new(compiled.program.with_local_count(compiled.locals)));
    store.vm_mut().set_fuel(100_000);
    store.run().unwrap();
    let state = Arc::new(CloseState::default());
    let error = invoke(&mut store, &state).unwrap_err();
    assert!(
        error.to_string().contains("ownership unresolved"),
        "{error}"
    );
    let frames = store.vm.instance.execution_frames.len();
    let locals = store.vm.instance.locals.clone();
    let run = store.run();
    let second = invoke(&mut store, &state);
    eprintln!(
        "callback_projection_source_graph admitted=true error={error} run={run:?} second={second:?} frames_before={frames} frames_after={} live={}",
        store.vm.instance.execution_frames.len(),
        store.vm.host_context().resource_count()
    );
    assert!(
        run.as_ref()
            .is_err_and(|error| error.to_string().contains("ownership unresolved")),
        "source-created graph must reject at ownership admission, got {run:?}"
    );
    assert_eq!(store.vm.instance.execution_frames.len(), frames);
    assert_eq!(store.vm.instance.locals, locals);
    assert!(second.is_err());
    assert_eq!(store.vm.host_context().resource_count(), 1);
    state.ready.store(true, Ordering::SeqCst);
    store.reset_for_reuse().unwrap();
    assert_eq!(store.vm.host_context().resource_count(), 0);
    store.run().unwrap();
}

#[test]
fn callback_publication_pending_reset_keeps_quarantine_until_real_replacement() {
    for success in [true, false] {
        let mut store = constant_store(success);
        let state = Arc::new(CloseState::default());
        assert!(
            invoke(&mut store, &state)
                .unwrap_err()
                .to_string()
                .contains("ownership unresolved")
        );
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            store.poll_reset_for_reuse(&mut cx),
            std::task::Poll::Ready(Ok(()))
        ));
        assert!(
            store.vm.instance.callback_publication_unresolved,
            "a poll without reset is not replacement"
        );
        assert!(store.run().is_err());
        store.reset_for_reuse().unwrap();
        assert!(store.vm().scope_reset_pending());
        assert!(
            store.vm.instance.callback_publication_unresolved,
            "interpreter reset cannot clear quarantine"
        );
        assert!(!store.is_reusable());
        assert_eq!(state.begins.load(Ordering::SeqCst), 1);
        assert_eq!(store.vm.host_context().resource_count(), 1);
        assert!(store.poll_reset_for_reuse(&mut cx).is_pending());
        assert!(store.vm.instance.callback_publication_unresolved);
        assert!(store.run().is_err());
        assert!(store.vm_mut().run().is_err());
        state.ready.store(true, Ordering::SeqCst);
        assert!(matches!(
            store.poll_reset_for_reuse(&mut cx),
            std::task::Poll::Ready(Ok(()))
        ));
        assert!(!store.vm.instance.callback_publication_unresolved);
        assert!(!store.vm().scope_reset_pending());
        assert!(store.is_reusable());
        assert_eq!(state.begins.load(Ordering::SeqCst), 1);
        assert!(state.polls.load(Ordering::SeqCst) >= 2);
        assert_eq!(state.drops.load(Ordering::SeqCst), 1);
        assert_eq!(store.vm.host_context().resource_count(), 0);
        store.run().unwrap();
        let ordinary = store
            .vm_script_callback_by_name::<(i64,), i64>("ordinary")
            .unwrap();
        assert_eq!(ordinary.call_with_vm(&mut store, (7,)).unwrap(), 8);
        eprintln!(
            "callback_quarantine_reset success={success} pending_preserved=true terminal_replacement=true close_begin=1 drops=1 ordinary_return=8"
        );
    }
}

#[test]
fn callback_publication_failed_cleanup_never_clears_quarantine() {
    let mut store = constant_store(true);
    let state = Arc::new(CloseState::default());
    assert!(invoke(&mut store, &state).is_err());
    state.fail.store(true, Ordering::SeqCst);
    let reset = store.reset_for_reuse().unwrap_err();
    assert!(store.vm.instance.callback_publication_unresolved);
    assert!(!store.is_reusable());
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(matches!(
        store.poll_reset_for_reuse(&mut cx),
        std::task::Poll::Ready(Err(_))
    ));
    assert!(store.vm.instance.callback_publication_unresolved);
    assert!(store.run().is_err());
    assert!(invoke(&mut store, &state).is_err());
    assert_eq!(state.begins.load(Ordering::SeqCst), 1);
    assert!(store.reset_for_reuse().is_err());
    assert!(store.vm.instance.callback_publication_unresolved);
    eprintln!(
        "callback_quarantine_failed_cleanup reset={reset} quarantine_retained=true close_begin=1"
    );
}

#[test]
fn callback_publication_pending_shutdown_stays_terminal_until_completed_reset() {
    let mut store = constant_store(true);
    let state = Arc::new(CloseState::default());
    assert!(invoke(&mut store, &state).is_err());
    store.vm_mut().shutdown();
    assert_eq!(store.vm.host_context().resource_count(), 1);
    assert!(store.vm.instance.callback_publication_unresolved);
    assert!(!store.is_reusable());
    assert!(store.run().is_err());
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(
        store
            .vm_mut()
            .execution_scope()
            .poll_close(&mut cx)
            .is_pending()
    );
    assert_eq!(state.begins.load(Ordering::SeqCst), 1);
    state.ready.store(true, Ordering::SeqCst);
    assert!(
        store
            .vm_mut()
            .execution_scope()
            .poll_close(&mut cx)
            .is_ready()
    );
    assert_eq!(state.begins.load(Ordering::SeqCst), 1);
    assert_eq!(state.drops.load(Ordering::SeqCst), 1);
    assert_eq!(store.vm.host_context().resource_count(), 0);
    assert!(
        store.vm.instance.callback_publication_unresolved,
        "a closed terminal scope is not a reusable replacement"
    );
    assert!(store.run().is_err());
    store.reset_for_reuse().unwrap();
    assert!(!store.vm.instance.callback_publication_unresolved);
    store.run().unwrap();
    let ordinary = store
        .vm_script_callback_by_name::<(i64,), i64>("ordinary")
        .unwrap();
    assert_eq!(ordinary.call_with_vm(&mut store, (10,)).unwrap(), 11);
    eprintln!(
        "callback_quarantine_shutdown pending_retained=true actual_poll_live=0 reset_replacement=true ordinary_return=11 close_begin=1"
    );
}
