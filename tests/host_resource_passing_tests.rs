use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use vm::execution_scope::ExecutionScopeError;
use vm::operation::OperationSpec;
use vm::resource::{ResourceCloseReason, ResourceErrorCode, ResourceResult};
use vm::{
    CloseProgress, HostResource, Program, ResourceAccessMode, ResourceAccessRequest,
    ResourceHandle, ResourceTable, ResourceTypeKey, Vm, VmError,
};

fn new_vm() -> Vm {
    Vm::try_new(Program::new(Vec::new(), Vec::new())).expect("test VM construction must not fail")
}

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

struct LegacyResource(i64);

impl HostResource for LegacyResource {}

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
    let mut table = ResourceTable::new().expect("table");
    let (resource, closes) = fake(7);
    let token = table.push(resource).expect("push");
    let handle = token.handle();
    table.mark_guest_owned(handle).expect("guest ownership");

    let requests = vec![ResourceAccessRequest::borrow_with_key::<FakeResource>(
        handle,
        fake_key(),
    )];
    let frame = table
        .begin_resource_access(requests)
        .expect("borrow preflight");
    let borrowed = frame.borrow::<FakeResource>(0).expect("borrow");
    assert_eq!(borrowed.value, 7);
    drop(borrowed);
    drop(frame);

    let requests = vec![ResourceAccessRequest::borrow_mut::<FakeResource>(handle)];
    let frame = table
        .begin_resource_access(requests)
        .expect("mutable borrow preflight");
    let mut borrowed = frame.borrow_mut::<FakeResource>(0).expect("mutable borrow");
    borrowed.value = 11;
    drop(borrowed);
    drop(frame);

    let requests = vec![ResourceAccessRequest::take_owned::<FakeResource>(handle)];
    let frame = table
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
    let mut table = ResourceTable::new().expect("table");
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
    let mut table = ResourceTable::new().expect("table");
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

    let frame = table
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
    let mut vm = Vm::try_new(Program::new(Vec::new(), Vec::new()))
        .expect("test VM construction must not fail");
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

    let mut vm = Vm::try_new(Program::new(Vec::new(), Vec::new()))
        .expect("test VM construction must not fail");
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
fn request_modes_keep_value_outside_the_resource_adapter() {
    // Exactly the four live modes exist on the host side. `ToOwned` is not a
    // host passing mode (a guest `to_owned()` is ordinary `Value` passing), so
    // there is nothing to alias: every variant maps 1:1 to a `HostParamPassing`
    // and `Value` remains the only non-resource placeholder.
    assert_eq!(ResourceAccessMode::Borrow.is_borrow(), true);
    assert_eq!(ResourceAccessMode::BorrowMut.is_mutable(), true);
    assert_eq!(ResourceAccessMode::TakeOwned.is_consuming(), true);
    assert_eq!(ResourceAccessMode::Value.is_consuming(), false);
    assert!(!ResourceAccessMode::Value.is_borrow());
    assert!(!ResourceAccessMode::Value.is_mutable());
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
        ResourceAccessMode::Value.host_param_passing(),
        Some(vm::HostParamPassing::Value)
    );
}

#[test]
fn resource_frame_rejects_repeated_mutable_borrow_for_one_request() {
    let mut table = ResourceTable::new().expect("table");
    let token = table.push(fake(1).0).expect("push");
    let frame = table
        .begin_resource_access(vec![ResourceAccessRequest::borrow_mut::<FakeResource>(
            token.handle(),
        )])
        .expect("preflight");

    let first = frame
        .borrow_mut::<FakeResource>(0)
        .expect("first mutable borrow");
    drop(first);
    let error = frame
        .borrow_mut::<FakeResource>(0)
        .expect_err("one request cannot mint a second mutable guard");
    assert_eq!(error.code(), ResourceErrorCode::ResourceAccessConflict);
}

#[test]
fn distinct_resource_requests_allow_multiple_mutable_guards() {
    let mut table = ResourceTable::new().expect("table");
    let first = table.push(fake(1).0).expect("first");
    let second = table.push(fake(2).0).expect("second");
    let frame = table
        .begin_resource_access(vec![
            ResourceAccessRequest::borrow_mut::<FakeResource>(first.handle()),
            ResourceAccessRequest::borrow_mut::<FakeResource>(second.handle()),
        ])
        .expect("preflight");

    let mut first_guard = frame
        .borrow_mut::<FakeResource>(0)
        .expect("first mutable guard");
    let mut second_guard = frame
        .borrow_mut::<FakeResource>(1)
        .expect("second mutable guard");
    first_guard.value += 10;
    second_guard.value += 20;
    assert_eq!(first_guard.value, 11);
    assert_eq!(second_guard.value, 22);
}

#[test]
fn explicit_key_mismatch_is_rejected_before_push_mutation() {
    let mut table = ResourceTable::new().expect("table");
    let error = table
        .push_with_key(fake(1).0, other_key())
        .expect_err("static resource key mismatch must reject the push");
    assert_eq!(error.code(), ResourceErrorCode::ResourceKeyMismatch);
    assert!(table.is_empty(), "rejected push must not allocate a slot");
}

#[test]
fn from_value_key_mismatch_stays_structured_in_vm_error() {
    let mut table = ResourceTable::new().expect("table");
    let token = table.push(fake(2).0).expect("push");
    let error = ResourceAccessRequest::from_value_with_key::<FakeResource>(
        &token.handle().as_value(),
        ResourceAccessMode::Borrow,
        other_key(),
        "test.arg",
    )
    .expect_err("request key must match the static resource key");
    assert_eq!(
        error.resource_error_code(),
        Some(ResourceErrorCode::ResourceKeyMismatch)
    );
    assert_eq!(
        error.resource_error().and_then(|error| error.value()),
        Some(token.handle().raw())
    );
}

#[test]
fn legacy_resource_requires_an_explicit_key_for_exact_frame_access() {
    let mut table = ResourceTable::new().expect("table");
    let key = ResourceTypeKey::new("test.legacy").expect("valid key");
    let token = table
        .push_with_key(LegacyResource(7), key.clone())
        .expect("legacy resources may declare a key at insertion");

    let no_key = ResourceAccessRequest::borrow::<LegacyResource>(token.handle());
    let no_key_error = table
        .begin_resource_access(vec![no_key])
        .expect_err("legacy exact access without a key must be rejected");
    assert_eq!(
        no_key_error.code(),
        ResourceErrorCode::ResourceKeyUnavailable
    );

    let request = ResourceAccessRequest::borrow_with_key::<LegacyResource>(token.handle(), key);
    let frame = table
        .begin_resource_access(vec![request])
        .expect("explicit legacy key should pass preflight");
    assert_eq!(frame.borrow::<LegacyResource>(0).expect("borrow").0, 7);
}

#[test]
fn host_context_mutable_resource_apis_use_mutable_requests() {
    let mut vm = new_vm();
    let token = vm.host_context().push_resource(fake(3).0).expect("push");
    {
        let mut context = vm.host_context();
        let mut resource = context.resource_mut(&token).expect("resource_mut");
        resource.value += 4;
    }
    {
        let mut context = vm.host_context();
        let mut resource = context
            .borrow_resource_mut::<FakeResource>(token.handle())
            .expect("borrow_resource_mut");
        resource.value += 5;
    }
    assert_eq!(
        vm.host_context().resource(&token).expect("read back").value,
        12
    );
}

#[test]
fn direct_host_context_take_rejects_associated_operation_without_consuming() {
    let mut vm = new_vm();
    let token = vm.host_context().push_resource(fake(9).0).expect("push");
    let handle = token.handle();
    vm.host_context()
        .mark_resource_guest_owned(handle)
        .expect("guest ownership");
    vm.host_context()
        .start_operation(OperationSpec::new(NoopOperation).with_resource(handle))
        .expect("operation");

    let error = vm
        .host_context()
        .take_resource::<FakeResource>(handle)
        .expect_err("associated operation must block direct take");
    let code = match error.kind() {
        vm::HostContextErrorKind::Scope(ExecutionScopeError::Resource(error)) => error.code(),
        other => panic!("expected structured resource error, got {other:?}"),
    };
    assert_eq!(code, ResourceErrorCode::ResourceOperationActive);
    assert_eq!(
        vm.host_context().resource_ownership(handle),
        Some(vm::ResourceOwnership::GuestOwned)
    );
}

#[test]
fn vm_resource_errors_keep_the_machine_readable_code() {
    let mut vm = new_vm();
    let token = vm.host_context().push_resource(fake(1).0).expect("push");
    let handle = token.handle();
    vm.host_context()
        .mark_resource_guest_owned(handle)
        .expect("guest ownership");
    vm.host_context()
        .start_operation(OperationSpec::new(NoopOperation).with_resource(handle))
        .expect("operation");

    let error = vm
        .begin_resource_access(vec![ResourceAccessRequest::take_owned::<FakeResource>(
            handle,
        )])
        .expect_err("operation-aware preflight must reject the take");
    assert!(!matches!(error, VmError::HostError(_)));
    assert_eq!(
        error.resource_error_code(),
        Some(ResourceErrorCode::ResourceOperationActive)
    );

    let value_request = ResourceAccessRequest::from_value::<FakeResource>(
        &handle.as_value(),
        ResourceAccessMode::Value,
        "value",
    )
    .expect("static-key request");
    let mode_error = vm
        .begin_resource_access(vec![value_request])
        .expect_err("value mode is not a frame access mode");
    assert_eq!(
        mode_error.resource_error_code(),
        Some(ResourceErrorCode::ResourceAccessModeUnsupported)
    );

    let type_error = vm
        .begin_resource_access(vec![ResourceAccessRequest::borrow::<OtherResource>(handle)])
        .expect_err("wrong resource type must stay structured");
    assert_eq!(
        type_error.resource_error_code(),
        Some(ResourceErrorCode::ResourceTypeMismatch)
    );
}

#[allow(dead_code)]
fn _keep_imports(_: ResourceHandle) {}
