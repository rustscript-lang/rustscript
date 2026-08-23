use std::sync::Arc;
use std::sync::Mutex;
use std::task::{Context, Poll};

use pd_host_function::pd_host_function;
use vm::compiler::{CompileSourceFileOptions, SourceFlavor};
use vm::resource::{CloseProgress, HostResource, ResourceCloseReason, ResourceResult};
// `take_arg` is the moving counterpart of `borrow_arg`; the generated mut
// wrappers reference it via `super::take_arg` so it must stay in scope even
// when the current fixtures only exercise the shared/borrowing decoders.
#[allow(unused_imports)]
use vm::{
    CallOutcome, CallReturn, HostApiBuilder, HostFunctionRegistry, HostFunctionSchema,
    HostParamSchema, HostTypeSchema, Program, Resource, ResourceAccessMode, ResourceAccessRequest,
    ResourceHandle, ResourceMut, ResourceOwned, ResourceRef, ResourceTypeKey, ResourceTypeSchema,
    Value, Vm, VmError, VmResult, VmStatus, borrow_arg, compile_source_with_flavor_and_options,
    take_arg,
};

static LAST_MAKE_HANDLE: Mutex<Option<i64>> = Mutex::new(None);

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

        #[pd_host_function(name = "test::combo")]
        /// Takes a resource after a prefix ordinary argument, returning their sum.
        fn combo(prefix: i64, resource: ResourceOwned<FakeResource>) -> i64 {
            prefix + resource.into_inner().0
        }

        #[pd_host_function(name = "test::take_n")]
        /// Takes a resource and validates a trailing ordinary argument.
        fn take_n(resource: ResourceOwned<FakeResource>, n: i64) -> i64 {
            resource.into_inner().0 + n
        }

        #[pd_host_function(name = "test::interleave")]
        /// Takes two resources around a middle ordinary argument.
        fn interleave(
            a: ResourceOwned<FakeResource>,
            n: i64,
            b: ResourceOwned<FakeResource>,
        ) -> i64 {
            a.into_inner().0 + n + b.into_inner().0
        }

        #[pd_host_function(name = "test::mixed")]
        /// Borrows and mutably borrows two resources around an ordinary arg.
        fn mixed(
            a: ResourceRef<'_, FakeResource>,
            n: i64,
            mut b: ResourceMut<'_, FakeResource>,
        ) -> i64 {
            b.0 += 1;
            a.0 + n + b.0
        }

        #[pd_host_function(name = "test::make")]
        /// Pushes a fake resource into the caller's scope and returns the owned handle.
        fn make(vm: &mut Vm, seed: i64) -> Resource<FakeResource> {
            let token = vm
                .host_context()
                .push_resource(FakeResource(seed))
                .expect("push resource");
            *LAST_MAKE_HANDLE.lock().unwrap() = Some(token.handle().raw() as i64);
            token
        }
    }
}

fn new_vm() -> Vm {
    Vm::try_new(Program::new(Vec::new(), Vec::new())).expect("test VM construction must not fail")
}

/// Pushes a resource and marks it guest-owned, returning its raw handle.
fn push_guest_owned(vm: &mut Vm, value: FakeResource) -> ResourceHandle {
    let token = vm.host_context().push_resource(value).unwrap();
    let handle = token.handle();
    vm.host_context()
        .mark_resource_guest_owned(handle)
        .expect("mark guest owned");
    handle
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

/// A prefix ordinary argument occupies args slot 0 while the resource is frame
/// slot 0; before the frame-index fix this read the wrong slot and failed.
#[test]
fn prefix_ordinary_argument_and_resource_map_to_their_own_slots() {
    let mut vm = new_vm();
    let handle = push_guest_owned(&mut vm, FakeResource(40));
    let result = adapter::runtime::combo(&mut vm, &[Value::Int(5), handle.as_value()]).unwrap();
    assert_eq!(result, 45);
}

/// A wrong-typed trailing ordinary argument must fail *before* the resource
/// take, leaving the resource GuestOwned (zero partial consumption).
#[test]
fn wrong_typed_trailing_ordinary_argument_leaves_resource_guest_owned() {
    let mut vm = new_vm();
    let handle = push_guest_owned(&mut vm, FakeResource(40));
    let error = adapter::runtime::take_n(&mut vm, &[handle.as_value(), Value::string("boom")])
        .expect_err("wrong-typed ordinary argument must fail");
    assert!(matches!(error, VmError::TypeMismatch("int")));
    assert_eq!(
        vm.host_context().resource_ownership(handle),
        Some(vm::ResourceOwnership::GuestOwned),
        "a failing ordinary argument must not consume the earlier resource"
    );
}

/// Multiple resources interleaved with ordinary arguments must each resolve to
/// their own frame slot (0, 1 here), not their argument index.
#[test]
fn multiple_resources_interleaved_with_ordinary_args_use_frame_slots() {
    let mut vm = new_vm();
    let a = push_guest_owned(&mut vm, FakeResource(40));
    let b = push_guest_owned(&mut vm, FakeResource(7));
    let result =
        adapter::runtime::interleave(&mut vm, &[a.as_value(), Value::Int(2), b.as_value()])
            .unwrap();
    assert_eq!(result, 49);
    assert_eq!(
        vm.host_context().resource_ownership(a),
        Some(vm::ResourceOwnership::Taken)
    );
    assert_eq!(
        vm.host_context().resource_ownership(b),
        Some(vm::ResourceOwnership::Taken)
    );

    let mut vm = new_vm();
    let a = vm
        .host_context()
        .push_resource(FakeResource(40))
        .unwrap()
        .handle();
    let b = vm
        .host_context()
        .push_resource(FakeResource(7))
        .unwrap()
        .handle();
    let result =
        adapter::runtime::mixed(&mut vm, &[a.as_value(), Value::Int(2), b.as_value()]).unwrap();
    assert_eq!(result, 50);
    assert_eq!(
        vm.host_context().resource_ownership(a),
        Some(vm::ResourceOwnership::HostOwned),
        "Borrow must not consume"
    );
    assert_eq!(
        vm.host_context().resource_ownership(b),
        Some(vm::ResourceOwnership::HostOwned),
        "BorrowMut must not consume"
    );
}

/// End-to-end owned `Resource<T>` return through the exact host-binding path:
/// the macro host function pushes into the caller's scope, converts the real
/// `Resource<T>` token to its `Value::Int` handle, and the C2-C1 exact-return
/// machinery marks the handle guest-owned during the real call.
#[test]
fn macro_generated_owned_resource_return_is_exactly_marked_guest_owned() {
    let key = ResourceTypeKey::new("test.fake").expect("valid key");
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(key.clone(), "fake"));
    builder.function(HostFunctionSchema::with_return(
        "acme::make",
        vec![HostParamSchema::value("seed", HostTypeSchema::Int)],
        HostTypeSchema::Resource(key),
    ));
    let catalog = Arc::new(builder.build().expect("catalog must build"));

    let source = "use acme;\nlet r = acme::make(7); r;\n";
    let compiled = compile_source_with_flavor_and_options(
        source,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(catalog),
    )
    .expect("catalog source should compile");
    let schema = compiled
        .program
        .imports
        .iter()
        .find(|import| import.name == "acme::make")
        .expect("make import")
        .schema
        .clone()
        .expect("exact schema");

    fn make_adapter(vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let token: Resource<FakeResource> = adapter::runtime::make(vm, args)?;
        // Convert the real owned handle token to its Value::Int handle.
        Ok(CallOutcome::Return(CallReturn::One(
            token.into_handle().as_value(),
        )))
    }

    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact_static_stack("acme::make", 1, schema, make_adapter)
        .expect("register exact make");
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    registry.bind_vm_cached(&mut vm).expect("bind");

    assert_eq!(vm.run().expect("run"), VmStatus::Halted);
    let raw = LAST_MAKE_HANDLE
        .lock()
        .unwrap()
        .expect("make pushed a resource");
    let handle = ResourceHandle::from_raw(raw as u64).expect("real handle");
    assert_eq!(
        vm.host_context()
            .execution_scope()
            .resources()
            .ownership(handle),
        Some(vm::ResourceOwnership::GuestOwned),
        "the exact Resource return must transfer the pushed resource to the guest"
    );
}
