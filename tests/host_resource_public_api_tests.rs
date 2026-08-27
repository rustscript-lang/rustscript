use vm::resource::{
    CloseProgress, HostResource, ResourceCloseReason, ResourceErrorCode, ResourceOwned,
};
use vm::{Program, Vm};

#[derive(Debug, PartialEq)]
struct Counter(i64);

impl HostResource for Counter {
    fn begin_close(
        &mut self,
        _reason: ResourceCloseReason,
    ) -> vm::resource::ResourceResult<CloseProgress> {
        Ok(CloseProgress::Ready)
    }
}

#[derive(Debug, PartialEq)]
struct Other;

impl HostResource for Other {}

#[test]
fn public_resource_owned_is_a_real_take_owned_value() {
    let owned = ResourceOwned::new(Counter(7));
    assert_eq!(owned.as_ref(), &Counter(7));
    assert_eq!(owned.into_inner(), Counter(7));
}

#[test]
fn host_context_resource_modes_and_take_are_public_and_typed() {
    let program = Program::new(Vec::new(), vec![vm::OpCode::Ret as u8]);
    let mut vm = Vm::new(program);
    let token = vm
        .host_context()
        .push_resource(Counter(7))
        .expect("resource registration");

    {
        let context = vm.host_context();
        assert_eq!(
            context
                .borrow_resource::<Counter>(token.handle())
                .expect("shared borrow")
                .0,
            7
        );
    }
    {
        let mut context = vm.host_context();
        context
            .borrow_resource_mut::<Counter>(token.handle())
            .expect("mutable borrow")
            .0 = 9;
    }

    let taken = vm
        .host_context()
        .take_resource::<Counter>(token.handle())
        .expect("take-owned");
    assert_eq!(taken, Counter(9));
    assert_eq!(vm.host_context().resource_count(), 0);

    let stale = vm
        .host_context()
        .take_resource::<Counter>(token.handle())
        .expect_err("a taken token cannot be taken twice");
    assert!(matches!(
        stale.kind(),
        vm::HostContextErrorKind::Scope(vm::execution_scope::ExecutionScopeError::Resource(_))
    ));
    if let vm::HostContextErrorKind::Scope(vm::execution_scope::ExecutionScopeError::Resource(
        resource_error,
    )) = stale.kind()
    {
        assert_eq!(
            resource_error.code(),
            ResourceErrorCode::ResourceAlreadyClosed
        );
    }
}

#[test]
fn public_take_rejects_wrong_type_without_removing_resource() {
    let program = Program::new(Vec::new(), vec![vm::OpCode::Ret as u8]);
    let mut vm = Vm::new(program);
    let token = vm
        .host_context()
        .push_resource(Counter(1))
        .expect("resource registration");

    let error = vm
        .host_context()
        .take_resource::<Other>(token.handle())
        .expect_err("wrong type");
    if let vm::HostContextErrorKind::Scope(vm::execution_scope::ExecutionScopeError::Resource(
        resource_error,
    )) = error.kind()
    {
        assert_eq!(
            resource_error.code(),
            ResourceErrorCode::ResourceTypeMismatch
        );
    } else {
        panic!("expected resource error, got {error:?}");
    }
    assert_eq!(vm.host_context().resource_count(), 1);
    assert_eq!(
        vm.host_context()
            .take_resource::<Counter>(token.handle())
            .expect("right type still succeeds"),
        Counter(1)
    );
}
