use super::*;
use crate::{HostResourceType, ResourceCloseReason, ResourceTypeKey};

#[derive(Debug)]
struct Item;
impl crate::resource::HostResource for Item {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(ResourceTypeKey::new(Self::KEY).unwrap())
    }
}
impl HostResourceType for Item {
    const KEY: &'static str = "sdk.abort_item";
    const DESCRIPTION: &'static str = "ownership projection test";
}
fn fixture() -> (Vm, Vec<PendingScriptResource>, ResourceHandle) {
    let mut vm = Vm::new(crate::Program::new(
        Vec::new(),
        vec![crate::OpCode::Ret as u8],
    ));
    let handle = vm.host_context().insert_resource(Item).unwrap().handle();
    let journal = vec![PendingScriptResource {
        handle,
        key: Item::KEY,
        close: super::super::close_unpublished::<Item>,
    }];
    (vm, journal, handle)
}
#[test]
fn callback_publication_shared_container_external_owner_and_typed_int_coincidence() {
    let (mut vm, journal, handle) = fixture();
    let value = crate::result::carrier(true, Value::Int(handle.raw() as i64));
    vm.instance
        .stack
        .push(Value::array(vec![value.clone(), value.clone()]));
    assert!(
        abandoned(&vm, &journal, 0, 0, 0).unwrap().is_empty(),
        "host-retained carrier is a real surviving Arc root"
    );
    drop(value);
    vm.instance.locals.push(Value::Int(handle.raw() as i64));
    let mut program = (*vm.program).clone();
    program.type_map = Some(crate::TypeMap {
        local_schemas: vec![Some(TypeSchema::Int)],
        ..Default::default()
    });
    vm.program = Arc::new(program);
    assert_eq!(
        abandoned(&vm, &journal, 0, 1, 0).unwrap(),
        vec![handle],
        "typed Int data is not a resource owner"
    );
    (journal[0].close)(&mut vm, handle).unwrap();
    assert_eq!(vm.host_context().resource_count(), 0);
}
#[test]
fn callback_publication_cycle_has_no_false_external_owner() {
    let (mut vm, journal, handle) = fixture();
    let cell = Arc::new(std::sync::Mutex::new(Value::Null));
    let weak_cell = Arc::downgrade(&cell);
    let env = Arc::new(CallableEnvironment {
        cells: std::sync::Mutex::new(vec![cell.clone()]),
    });
    let callable = Arc::new(crate::CallableValue {
        prototype_id: 0,
        kind: crate::CallableKind::Closure,
        env: Some(env),
    });
    *cell.lock().unwrap() = Value::array(vec![
        Value::Int(handle.raw() as i64),
        Value::Callable(callable.clone()),
    ]);
    vm.instance.owned_callables.push(Arc::downgrade(&callable));
    vm.instance.stack.push(Value::Callable(callable));
    drop(cell);
    assert_eq!(abandoned(&vm, &journal, 0, 0, 0).unwrap(), vec![handle]);
    *weak_cell.upgrade().unwrap().lock().unwrap() = Value::Null;
    (journal[0].close)(&mut vm, handle).unwrap();
    assert_eq!(vm.host_context().resource_count(), 0);
}
#[test]
fn callback_publication_surviving_queue_host_return_and_iterator_roots() {
    let (mut vm, journal, handle) = fixture();
    let encoded = Value::Int(handle.raw() as i64);
    vm.instance.stack.push(encoded.clone());
    vm.instance.host_return = Some(encoded.clone());
    assert!(abandoned(&vm, &journal, 0, 0, 0).unwrap().is_empty());
    vm.instance.host_return = None;
    vm.instance
        .queued_callables
        .push_back(crate::vm::instance::QueuedCallable {
            callable: Value::Null,
            args: vec![encoded.clone()],
            subscription: None,
        });
    assert!(abandoned(&vm, &journal, 0, 0, 0).unwrap().is_empty());
    vm.instance.queued_callables.clear();
    let map = Value::map(vec![(Value::string("resource"), encoded)]);
    let Value::Map(map) = map else { unreachable!() };
    vm.instance
        .map_iterators
        .push(vec![Some(crate::vm::map_iter::MapIteratorState::new(map))]);
    assert!(abandoned(&vm, &journal, 0, 0, 0).unwrap().is_empty());
    vm.instance.map_iterators.clear();
    assert_eq!(abandoned(&vm, &journal, 0, 0, 0).unwrap(), vec![handle]);
    vm.host_context()
        .close_resource::<Item>(handle, ResourceCloseReason::Requested)
        .unwrap();
}
#[test]
fn callback_publication_projection_failure_retains_actual_execution_roots() {
    let compiled = crate::compile_source_for_repl("pub fn fail(value: resource<sdk.abort_item>) -> resource<sdk.abort_item> { let n = 1 / 0; value }").unwrap();
    let mut program = compiled.program.with_local_count(compiled.locals);
    let mut deep = Value::Null;
    for _ in 0..MAX_DEPTH + 1 {
        deep = Value::array(vec![deep]);
    }
    program.constants.push(deep);
    let mut store = crate::Store::from_vm(Vm::new(program));
    store.run().unwrap();
    let callback = store
        .vm_script_callback_by_name::<(crate::ResourceOwned<Item>,), crate::ResourceOwned<Item>>(
            "fail",
        )
        .unwrap();
    let error = callback
        .call_with_vm(&mut store, (crate::ResourceOwned::new(Item),))
        .unwrap_err();
    assert!(
        error.to_string().contains("ownership unresolved"),
        "{error}"
    );
    assert_eq!(store.vm.instance.execution_frames.len(), 1);
    assert_eq!(store.vm.host_context().resource_count(), 1);
    assert!(
        store
            .vm
            .instance
            .locals
            .iter()
            .any(|value| matches!(value, Value::Int(_)))
    );
    let before_locals = store.vm.instance.locals.clone();
    let before_ip = store.vm.instance.ip;
    let run = store.run();
    let frames_after_run = store.vm.instance.execution_frames.len();
    let host_return_after_run = store.vm.instance.host_return.clone();
    let direct_run = store.vm_mut().run();
    let second = callback.call_with_vm(&mut store, (crate::ResourceOwned::new(Item),));
    eprintln!(
        "callback_projection_public_run unit_constant_fixture=true run={run:?} direct_run={direct_run:?} frames_after_run={frames_after_run} host_return_after_run={host_return_after_run:?} second={second:?} live={}",
        store.vm.host_context().resource_count()
    );
    assert!(
        run.is_err(),
        "Store::run must not resume unresolved ownership"
    );
    assert!(
        direct_run.is_err(),
        "Vm::run must not resume unresolved ownership"
    );
    assert_eq!(frames_after_run, 1);
    assert_eq!(host_return_after_run, None);
    assert_eq!(store.vm.instance.locals, before_locals);
    assert_eq!(store.vm.instance.ip, before_ip);
    assert!(
        second.is_err(),
        "no second publication into unresolved execution"
    );
    assert_eq!(store.vm.host_context().resource_count(), 1);
    store
        .vm
        .execution_scope()
        .begin_close(ResourceCloseReason::Requested)
        .unwrap();
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(store.vm.execution_scope().poll_close(&mut cx).is_ready());
    assert_eq!(store.vm.host_context().resource_count(), 0);
    store.vm.abort_host_invocation(0, 0);
    eprintln!(
        "callback_projection_limit execution_roots_retained=1 publications_before_reset=1 publications_after_scope_poll=0 second_publication_rejected=true"
    );
}

#[test]
fn callback_publication_typed_queued_int_is_data_not_an_owned_resource() {
    let compiled = crate::compile_source_for_repl("pub fn numeric(value: int) -> int { value } pub fn owned(value: resource<sdk.abort_item>) -> resource<sdk.abort_item> { value }").unwrap();
    let mut store =
        crate::Store::from_vm(Vm::new(compiled.program.with_local_count(compiled.locals)));
    store.run().unwrap();
    let numeric = store
        .script_callback_by_name::<(i64,), i64>("numeric")
        .unwrap();
    let (journal, handle) = {
        let mut context = crate::ScriptValueContext {
            vm: &mut store.vm,
            pending: Vec::new(),
        };
        let encoded = context
            .publish_owned(crate::ResourceOwned::new(Item))
            .unwrap();
        let handle = crate::result::resource_handle(&encoded).unwrap();
        context.vm.instance.stack.push(encoded);
        (context.pending, handle)
    };
    store
        .enqueue_callback(numeric.prepare((handle.raw() as i64,)).unwrap())
        .unwrap();
    assert_eq!(
        abandoned(&store.vm, &journal, 0, store.vm.instance.locals.len(), 0).unwrap(),
        vec![handle],
        "the admitted queued parameter schema is Int, not Resource"
    );
    store.vm.instance.queued_callables.clear();
    let owned = store
        .script_callback_by_name::<Vec<Value>, Value>("owned")
        .unwrap();
    store
        .enqueue_callback(
            owned
                .prepare(vec![Value::Int(handle.raw() as i64)])
                .unwrap(),
        )
        .unwrap();
    assert!(
        abandoned(&store.vm, &journal, 0, store.vm.instance.locals.len(), 0)
            .unwrap()
            .is_empty(),
        "the admitted Resource parameter is a surviving owner"
    );
    store.vm.instance.queued_callables.clear();
    (journal[0].close)(&mut store.vm, handle).unwrap();
    assert_eq!(store.vm.host_context().resource_count(), 0);
}

#[test]
fn callback_publication_graph_limit_is_explicit_without_resource_close() {
    let (mut vm, journal, handle) = fixture();
    let mut value = Value::Int(handle.raw() as i64);
    for _ in 0..MAX_DEPTH + 1 {
        value = Value::array(vec![value]);
    }
    vm.instance.stack.push(value);
    let error = abandoned(&vm, &journal, 0, 0, 0).unwrap_err();
    assert!(error.to_string().contains("ownership graph limit"));
    assert_eq!(vm.host_context().resource_count(), 1);
    vm.host_context()
        .close_resource::<Item>(handle, ResourceCloseReason::Requested)
        .unwrap();
}
