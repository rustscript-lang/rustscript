use std::task::{Context, Poll};

use pd_host_function::pd_host_function;
use vm::resource::{CloseProgress, ResourceCloseReason, ResourceResult};
use vm::{
    HostResource, Program, ResourceAccessMode, ResourceAccessRequest, ResourceMut, ResourceOwned,
    ResourceRef, ResourceTypeKey, Value, Vm, VmError, VmResult,
};

#[derive(Debug)]
struct FakeResource(i64);

impl HostResource for FakeResource {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(ResourceTypeKey::new("test.fake").unwrap())
    }

    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        Ok(CloseProgress::Ready)
    }

    fn poll_close(&mut self, _cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        Poll::Ready(Ok(()))
    }
}

pub mod adapter {
    use super::*;

    pub mod runtime {
        use super::*;

        #[pd_host_function(name = "test::borrow")]
        /// Borrows a fake resource.
        fn borrow(resource: ResourceRef<'_, FakeResource>) -> i64 {
            resource.0
        }

        #[pd_host_function(name = "test::bump")]
        /// Mutably borrows a fake resource.
        fn bump(mut resource: ResourceMut<'_, FakeResource>) -> i64 {
            resource.0 += 1;
            resource.0
        }

        #[pd_host_function(name = "test::take")]
        /// Takes ownership of a fake resource.
        fn take(resource: ResourceOwned<FakeResource>) -> i64 {
            resource.into_inner().0
        }

        #[pd_host_function(name = "test::take_explicit")]
        /// Takes an explicitly declared resource parameter.
        fn take_explicit(
            #[pd_host_param(passing = "take_owned", key = "test.fake")] resource: FakeResource,
        ) -> i64 {
            resource.0
        }

        #[pd_host_function(name = "test::panic_take")]
        /// Panics after an owned resource has been transferred.
        fn panic_take(_resource: ResourceOwned<FakeResource>) -> i64 {
            panic!("test host panic")
        }
    }
}

fn new_vm() -> Vm {
    Vm::new(Program::new(Vec::new(), Vec::new()))
}

#[test]
fn generated_resource_adapters_borrow_mutate_take_and_reject_stale_handle() {
    let mut vm = new_vm();
    let handle = vm
        .host_context()
        .push_resource(FakeResource(40))
        .unwrap()
        .handle();

    let borrowed = adapter::runtime::borrow(&mut vm, &[handle.as_value()]).unwrap();
    assert_eq!(borrowed, 40);
    let mutated = adapter::runtime::bump(&mut vm, &[handle.as_value()]).unwrap();
    assert_eq!(mutated, 41);

    vm.host_context().mark_resource_guest_owned(handle).unwrap();
    let taken = adapter::runtime::take(&mut vm, &[handle.as_value()]).unwrap();
    assert_eq!(taken, 41);
    let stale = vm.host_context().typed_resource::<FakeResource>(handle);
    assert!(stale.is_err(), "a taken raw handle must be rejected");

    let mut vm = new_vm();
    let handle = vm
        .host_context()
        .push_resource(FakeResource(12))
        .unwrap()
        .handle();
    vm.host_context().mark_resource_guest_owned(handle).unwrap();
    assert_eq!(
        adapter::runtime::take_explicit(&mut vm, &[handle.as_value()]).unwrap(),
        12
    );
}

#[test]
fn generated_take_owned_marks_taken_before_host_panic() {
    let mut vm = new_vm();
    let handle = vm
        .host_context()
        .push_resource(FakeResource(9))
        .unwrap()
        .handle();
    vm.host_context().mark_resource_guest_owned(handle).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = adapter::runtime::panic_take(&mut vm, &[handle.as_value()]);
    }));
    assert!(result.is_err());
    assert_eq!(
        vm.host_context().resource_ownership(handle),
        Some(vm::ResourceOwnership::Taken)
    );
}
