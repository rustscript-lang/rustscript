use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use vm::execution_scope::ExecutionScopeError;
use vm::operation::OperationSpec;
use vm::resource::{ResourceCloseReason, ResourceErrorCode, ResourceResult};
use vm::{
    CloseProgress, HostResource, Program, ResourceAccessMode, ResourceAccessRequest,
    ResourceHandle, ResourceTable, ResourceTypeKey, Vm,
};

fn fake_key() -> ResourceTypeKey {
    ResourceTypeKey::new("test.fake").expect("valid key")
}

fn other_key() -> ResourceTypeKey {
    ResourceTypeKey::new("test.other").expect("valid key")
}

#[derive(Debug)]
struct FakeResource {
    value: i64,
    closes: Arc<AtomicUsize>,
}

impl HostResource for FakeResource {
    fn resource_type_key() -> Option<ResourceTypeKey>
    where
        Self: Sized,
    {
        Some(fake_key())
    }

    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        Ok(CloseProgress::Ready)
    }
}

#[derive(Debug)]
struct OtherResource;

impl HostResource for OtherResource {
    fn resource_type_key() -> Option<ResourceTypeKey>
    where
        Self: Sized,
    {
        Some(other_key())
    }
}

fn fake(value: i64) -> (FakeResource, Arc<AtomicUsize>) {
    let closes = Arc::new(AtomicUsize::new(0));
    (
        FakeResource {
            value,
            closes: closes.clone(),
        },
        closes,
    )
}

#[test]
fn raw_handle_frame_supports_borrow_mut_and_take_owned_then_rejects_old_handle() {
    let mut table = ResourceTable::new();
    let (resource, closes) = fake(7);
    let token = table.push(resource).expect("push");
    let handle = token.handle();
    table.mark_guest_owned(handle).expect("guest ownership");

    let requests = vec![ResourceAccessRequest::borrow_with_key::<FakeResource>(
        handle,
        fake_key(),
    )];
    let mut frame = table
        .begin_resource_access(requests)
        .expect("borrow preflight");
    let borrowed = frame.borrow::<FakeResource>(0).expect("borrow");
    assert_eq!(borrowed.value, 7);
    let _ = borrowed;
    drop(frame);

    let requests = vec![ResourceAccessRequest::borrow_mut::<FakeResource>(handle)];
    let mut frame = table
        .begin_resource_access(requests)
        .expect("mutable borrow preflight");
    let mut borrowed = frame.borrow_mut::<FakeResource>(0).expect("mutable borrow");
    borrowed.value = 11;
    let _ = borrowed;
    drop(frame);

    let requests = vec![ResourceAccessRequest::take_owned::<FakeResource>(handle)];
    let mut frame = table
        .begin_resource_access(requests)
        .expect("take preflight");
    let owned = frame.take_owned::<FakeResource>(0).expect("take");
    assert_eq!(owned.value, 11);
    drop(frame);
    assert_eq!(
        closes.load(Ordering::SeqCst),
        0,
        "taken values are not closed by scope exit"
    );
    assert_eq!(
        table.typed::<FakeResource>(handle).unwrap_err().code(),
        ResourceErrorCode::ResourceAlreadyClosed
    );
}

#[test]
fn wrong_type_or_key_and_late_bad_argument_leave_every_take_unconsumed() {
    let mut table = ResourceTable::new();
    let (resource, _) = fake(1);
    let first = table.push(resource).expect("push");
    let first_handle = first.handle();
    table
        .mark_guest_owned(first_handle)
        .expect("guest ownership");

    let wrong_type = ResourceAccessRequest::take_owned::<OtherResource>(first_handle);
    let error = table.begin_resource_access(vec![wrong_type]).unwrap_err();
    assert_eq!(error.code(), ResourceErrorCode::ResourceTypeMismatch);
    assert_eq!(
        table.ownership(first_handle),
        Some(vm::ResourceOwnership::GuestOwned)
    );

    let wrong_key =
        ResourceAccessRequest::take_owned_with_key::<FakeResource>(first_handle, other_key());
    let error = table.begin_resource_access(vec![wrong_key]).unwrap_err();
    assert_eq!(error.code(), ResourceErrorCode::ResourceKeyMismatch);
    assert_eq!(
        table.ownership(first_handle),
        Some(vm::ResourceOwnership::GuestOwned)
    );

    let second = table.push(fake(2).0).expect("push second");
    let second_handle = second.handle();
    table
        .mark_guest_owned(second_handle)
        .expect("guest ownership second");
    let error = table
        .begin_resource_access(vec![
            ResourceAccessRequest::take_owned::<FakeResource>(first_handle),
            ResourceAccessRequest::take_owned::<OtherResource>(second_handle),
        ])
        .unwrap_err();
    assert_eq!(error.code(), ResourceErrorCode::ResourceTypeMismatch);
    assert_eq!(
        table.ownership(first_handle),
        Some(vm::ResourceOwnership::GuestOwned)
    );
    assert_eq!(
        table.ownership(second_handle),
        Some(vm::ResourceOwnership::GuestOwned)
    );
}

#[test]
fn alias_rules_are_checked_before_any_take_and_shared_borrows_are_allowed() {
    let mut table = ResourceTable::new();
    let (resource, _) = fake(3);
    let token = table.push(resource).expect("push");
    let handle = token.handle();
    table.mark_guest_owned(handle).expect("guest ownership");

    for requests in [
        vec![
            ResourceAccessRequest::take_owned::<FakeResource>(handle),
            ResourceAccessRequest::take_owned::<FakeResource>(handle),
        ],
        vec![
            ResourceAccessRequest::take_owned::<FakeResource>(handle),
            ResourceAccessRequest::borrow::<FakeResource>(handle),
        ],
        vec![
            ResourceAccessRequest::borrow_mut::<FakeResource>(handle),
            ResourceAccessRequest::borrow::<FakeResource>(handle),
        ],
    ] {
        let error = table.begin_resource_access(requests).unwrap_err();
        assert_eq!(error.code(), ResourceErrorCode::ResourceAccessConflict);
        assert_eq!(
            table.ownership(handle),
            Some(vm::ResourceOwnership::GuestOwned)
        );
    }

    let mut frame = table
        .begin_resource_access(vec![
            ResourceAccessRequest::borrow::<FakeResource>(handle),
            ResourceAccessRequest::borrow::<FakeResource>(handle),
        ])
        .expect("shared immutable borrows are legal");
    let first = frame.borrow::<FakeResource>(0).expect("first borrow");
    let second = frame.borrow::<FakeResource>(1).expect("second borrow");
    assert_eq!(first.value, second.value);
}

#[test]
fn take_owned_rejects_children_and_associated_operations_without_consuming() {
    let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));
    let parent = vm.host_context().push_resource(fake(10).0).expect("parent");
    vm.host_context()
        .push_child_resource(fake(20).0, &parent)
        .expect("child");
    let parent_handle = parent.handle();
    vm.host_context()
        .mark_resource_guest_owned(parent_handle)
        .expect("guest ownership");
    let error = vm
        .host_context()
        .begin_resource_access(vec![ResourceAccessRequest::take_owned::<FakeResource>(
            parent_handle,
        )])
        .unwrap_err();
    let code = match error.kind() {
        vm::HostContextErrorKind::Scope(ExecutionScopeError::Resource(error)) => error.code(),
        other => panic!("expected resource error, got {other:?}"),
    };
    assert_eq!(code, ResourceErrorCode::ResourceHasChildren);
    assert_eq!(
        vm.host_context().resource_ownership(parent_handle),
        Some(vm::ResourceOwnership::GuestOwned)
    );

    let mut vm = Vm::new(Program::new(Vec::new(), Vec::new()));
    let token = vm
        .host_context()
        .push_resource(fake(30).0)
        .expect("resource");
    let handle = token.handle();
    vm.host_context()
        .mark_resource_guest_owned(handle)
        .expect("guest ownership");
    vm.host_context()
        .start_operation(OperationSpec::new(NoopOperation).with_resource(handle))
        .expect("operation");
    let error = vm
        .host_context()
        .begin_resource_access(vec![ResourceAccessRequest::take_owned::<FakeResource>(
            handle,
        )])
        .unwrap_err();
    let code = match error.kind() {
        vm::HostContextErrorKind::Scope(ExecutionScopeError::Resource(error)) => error.code(),
        other => panic!("expected resource error, got {other:?}"),
    };
    assert_eq!(code, ResourceErrorCode::ResourceOperationActive);
    assert_eq!(
        vm.host_context().resource_ownership(handle),
        Some(vm::ResourceOwnership::GuestOwned)
    );
}

struct NoopOperation;

impl vm::operation::HostOperation for NoopOperation {
    fn poll(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<vm::operation::OperationResult<()>> {
        std::task::Poll::Pending
    }

    fn cancel(
        &mut self,
        _reason: vm::operation::OperationCancelReason,
    ) -> vm::operation::OperationResult<()> {
        Ok(())
    }
}

#[test]
fn request_modes_keep_value_and_to_owned_outside_resource_adapter() {
    assert_eq!(ResourceAccessMode::Borrow.is_borrow(), true);
    assert_eq!(ResourceAccessMode::BorrowMut.is_mutable(), true);
    assert_eq!(ResourceAccessMode::TakeOwned.is_consuming(), true);
    assert_eq!(
        ResourceAccessMode::Borrow.host_param_passing(),
        Some(vm::HostParamPassing::Borrow)
    );
    assert_eq!(
        ResourceAccessMode::BorrowMut.host_param_passing(),
        Some(vm::HostParamPassing::BorrowMut)
    );
    assert_eq!(
        ResourceAccessMode::TakeOwned.host_param_passing(),
        Some(vm::HostParamPassing::TakeOwned)
    );
    assert_eq!(
        ResourceAccessMode::ToOwned.host_param_passing(),
        Some(vm::HostParamPassing::Value)
    );
}

#[allow(dead_code)]
fn _keep_imports(_: ResourceHandle) {}
