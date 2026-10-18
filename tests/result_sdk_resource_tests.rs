#![cfg(feature = "runtime")]

use pd_host_function::pd_host_function;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use vm::resource::{CloseProgress, HostResource, ResourceCloseReason, ResourceResult};
use vm::{
    GuestResult, HostResourceType, ResourceOwned, ResourceTypeKey, Value, Vm, VmResult, VmStatus,
};
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
#[path = "support/result_sdk_abort.rs"]
mod abort;
#[path = "support/result_sdk_success.rs"]
mod success;
static CLOSED: AtomicUsize = AtomicUsize::new(0);
static CREATED_OK: AtomicUsize = AtomicUsize::new(0);
static CREATED_ERR: AtomicUsize = AtomicUsize::new(0);
static HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
static OBSERVED_RESOURCE_COUNT: AtomicUsize = AtomicUsize::new(0);
static OBSERVATIONS: AtomicUsize = AtomicUsize::new(0);
static CONVERSIONS: AtomicUsize = AtomicUsize::new(0);
static DROPPED_OK: AtomicUsize = AtomicUsize::new(0);
static DROPPED_ERR: AtomicUsize = AtomicUsize::new(0);
static PENDING_READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static CLOSE_POLLS: AtomicUsize = AtomicUsize::new(0);
#[derive(Debug)]
struct Success(i64);
#[derive(Debug)]
struct Failure(i64);
impl Drop for Success {
    fn drop(&mut self) {
        DROPPED_OK.fetch_add(1, Ordering::SeqCst);
    }
}
impl Drop for Failure {
    fn drop(&mut self) {
        DROPPED_ERR.fetch_add(1, Ordering::SeqCst);
    }
}
macro_rules! resource_type {
    ($ty:ident, $key:literal) => {
        impl HostResource for $ty {
            fn resource_type_key() -> Option<ResourceTypeKey> {
                Some(ResourceTypeKey::new($key).unwrap())
            }
            fn begin_close(&mut self, _: ResourceCloseReason) -> ResourceResult<CloseProgress> {
                CLOSED.fetch_add(1, Ordering::SeqCst);
                match self.0 {
                    -101 => Ok(CloseProgress::Pending),
                    -102 => Err(vm::resource::ResourceError::new(
                        vm::resource::ResourceErrorCode::ResourceCleanupFailed,
                        "sdk_abort_test",
                        "intentional close failure",
                    )),
                    _ => Ok(CloseProgress::Ready),
                }
            }
            fn poll_close(
                &mut self,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<ResourceResult<()>> {
                CLOSE_POLLS.fetch_add(1, Ordering::SeqCst);
                if PENDING_READY.load(Ordering::SeqCst) {
                    std::task::Poll::Ready(Ok(()))
                } else {
                    std::task::Poll::Pending
                }
            }
        }
        impl HostResourceType for $ty {
            const KEY: &'static str = $key;
            const DESCRIPTION: &'static str = $key;
        }
    };
}
resource_type!(Success, "result.success");
resource_type!(Failure, "result.failure");
mod generated {
    pub use vm::borrow_arg;
    pub mod functions {
        use super::super::*;
        include!("fixtures/result_sdk_resource_signature.rs");
        /// Observes publication before the RSS callback returns ownership.
        #[pd_host_function(name = "result_resource::observe")]
        fn observe(vm: &mut Vm) -> i64 {
            let count = vm.host_context().resource_count();
            OBSERVED_RESOURCE_COUNT.store(count, Ordering::SeqCst);
            OBSERVATIONS.fetch_add(1, Ordering::SeqCst);
            count as i64
        }
        /// Keeps fatal callback transport separate from the guest union.
        #[pd_host_function(name = "result_resource::fatal")]
        fn fatal() -> VmResult<i64> {
            Err(vm::VmError::HostError("callback transport failure".into()))
        }
        /// Takes ownership into persistent opaque Rust module state.
        #[pd_host_function(name = "result_resource::retain")]
        fn retain(
            vm: &mut Vm,
            value: GuestResult<ResourceOwned<Success>, ResourceOwned<Failure>>,
        ) -> i64 {
            vm.host_context().set_module_state(value);
            1
        }
        /// Takes the fixture's initial capture without a guest MoveLocal detach.
        #[pd_host_function(name = "result_resource::retire_initial")]
        fn retire_initial(vm: &mut Vm) -> VmResult<i64> {
            super::super::abort::retire_initial(vm)
        }
    }
}
include!(concat!(
    env!("OUT_DIR"),
    "/result_sdk_resource_build_schemas.rs"
));

#[test]
fn actual_resource_build_fixture_matches_generated_schema_passing_and_effects() {
    for (descriptor, (schema, effects, binding)) in [
        (
            generated::functions::make_descriptor(),
            result_sdk_build_make(),
        ),
        (
            generated::functions::take_descriptor(),
            result_sdk_build_take(),
        ),
        (
            generated::functions::borrow_descriptor(),
            result_sdk_build_borrow(),
        ),
        (
            generated::functions::bump_descriptor(),
            result_sdk_build_bump(),
        ),
    ] {
        assert_eq!(schema, descriptor.schema);
        assert_eq!(effects, descriptor.effects);
        assert_eq!(binding, descriptor.binding.kind);
    }
}

fn descriptors() -> Vec<vm::HostFunctionDescriptor> {
    vec![
        generated::functions::make_descriptor(),
        generated::functions::take_descriptor(),
        generated::functions::observe_descriptor(),
        generated::functions::fatal_descriptor(),
        generated::functions::borrow_descriptor(),
        generated::functions::retain_descriptor(),
        generated::functions::retire_initial_descriptor(),
        abort::suspend_descriptor(),
    ]
}
fn machine(source: &str) -> Vm {
    let descriptors = descriptors();
    let catalog = Arc::new(vm::HostFunctionDescriptor::collect_catalog(&descriptors).unwrap());
    let compiled = vm::compile_source_with_flavor_and_options(
        source,
        vm::SourceFlavor::RustScript,
        vm::CompileSourceFileOptions::default().with_host_api_catalog(catalog),
    )
    .unwrap();
    let mut registry = vm::HostFunctionRegistry::empty();
    registry.set_capability_profile(vm::CapabilityProfile::deny_all());
    vm::HostModuleDescriptor::install_descriptors(&mut registry, &descriptors).unwrap();
    for descriptor in &descriptors {
        registry.authorize_registered_builtin_import(&descriptor.schema.name);
    }
    let mut machine = Vm::new(compiled.program.with_local_count(compiled.locals));
    registry.bind_vm_cached(&mut machine).unwrap();
    machine.set_fuel(100_000);
    machine
}
fn empty_vm() -> Vm {
    Vm::new(vm::Program::new(Vec::new(), vec![vm::OpCode::Ret as u8]))
}

type OwnedResult = GuestResult<ResourceOwned<Success>, ResourceOwned<Failure>>;
fn callback_store() -> vm::Store {
    let mut store = vm::Store::from_vm(machine(
        r#"use result_resource;
        pub fn echo(value: Result<resource<result.success>, resource<result.failure>>) -> Result<resource<result.success>, resource<result.failure>> { result_resource::observe(); value }
        pub fn pair(value: Result<resource<result.success>, resource<result.failure>>, flag: int) -> Result<resource<result.success>, resource<result.failure>> { result_resource::observe(); value }
        pub fn replace(value: Result<resource<result.success>, resource<result.failure>>) -> Result<resource<result.success>, resource<result.failure>> { result_resource::take(value); result_resource::make(false) }
        pub fn fail(value: Result<resource<result.success>, resource<result.failure>>) -> Result<resource<result.success>, resource<result.failure>> { result_resource::take(value); result_resource::fatal(); result_resource::make(true) }
        pub fn fail_before_move(value: Result<resource<result.success>, resource<result.failure>>) -> Result<resource<result.success>, resource<result.failure>> { result_resource::observe(); result_resource::fatal(); value }
        pub fn retain_before_fatal(value: Result<resource<result.success>, resource<result.failure>>) -> Result<resource<result.success>, resource<result.failure>> { result_resource::retain(value); result_resource::fatal(); result_resource::make(true) }
        pub fn fuel_before_move(value: Result<resource<result.success>, resource<result.failure>>) -> Result<resource<result.success>, resource<result.failure>> { result_resource::observe(); let mut n = 0; while n < 1000 { n = n + 1; } value }
        pub fn suspend_before_move(value: Result<resource<result.success>, resource<result.failure>>, mode: int) -> Result<resource<result.success>, resource<result.failure>> { result_resource::observe(); result_resource::suspend(mode); value }
        pub fn fail_pair(first: Result<resource<result.success>, resource<result.failure>>, second: Result<resource<result.success>, resource<result.failure>>) -> Result<resource<result.success>, resource<result.failure>> { result_resource::observe(); result_resource::fatal(); result_resource::borrow(second); first }
        pub fn return_first(first: Result<resource<result.success>, resource<result.failure>>, second: Result<resource<result.success>, resource<result.failure>>) -> Result<resource<result.success>, resource<result.failure>> { result_resource::observe(); first }
        pub fn fresh_return(value: Result<resource<result.success>, resource<result.failure>>, flag: bool) -> Result<resource<result.success>, resource<result.failure>> { result_resource::observe(); result_resource::make(flag) }
        pub fn return_first_three(first: Result<resource<result.success>, resource<result.failure>>, second: Result<resource<result.success>, resource<result.failure>>, third: Result<resource<result.success>, resource<result.failure>>) -> Result<resource<result.success>, resource<result.failure>> { result_resource::observe(); first }
        pub fn retain_success(value: Result<resource<result.success>, resource<result.failure>>) -> int { result_resource::retain(value) }
        pub fn nullable(value: Result<resource<result.success>?, resource<result.failure>>) -> Result<resource<result.success>?, resource<result.failure>> { result_resource::observe(); value }
    "#,
    ));
    store.run().unwrap();
    store
}

#[test]
fn vm_aware_owned_callback_both_branches_publish_and_take() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut store = callback_store();
    let callback = store
        .vm_script_callback_by_name::<(OwnedResult,), OwnedResult>("echo")
        .unwrap();
    let closed = CLOSED.load(Ordering::SeqCst);
    let dropped_ok = DROPPED_OK.load(Ordering::SeqCst);
    let dropped_err = DROPPED_ERR.load(Ordering::SeqCst);
    for branch in [true, false] {
        let value = if branch {
            GuestResult::Ok(ResourceOwned::new(Success(61)))
        } else {
            GuestResult::Err(ResourceOwned::new(Failure(73)))
        };
        let returned = callback.call_with_vm(&mut store, (value,)).unwrap();
        assert_eq!(OBSERVED_RESOURCE_COUNT.load(Ordering::SeqCst), 1);
        assert_eq!(store.vm_mut().host_context().resource_count(), 0);
        match returned {
            GuestResult::Ok(value) => {
                assert!(branch);
                let mut value = value.into_inner();
                assert_eq!(value.0, 61);
                value.begin_close(ResourceCloseReason::Requested).unwrap();
            }
            GuestResult::Err(value) => {
                assert!(!branch);
                let mut value = value.into_inner();
                assert_eq!(value.0, 73);
                value.begin_close(ResourceCloseReason::Requested).unwrap();
            }
        }
    }
    assert_eq!(CLOSED.load(Ordering::SeqCst), closed + 2);
    assert_eq!(DROPPED_OK.load(Ordering::SeqCst), dropped_ok + 1);
    assert_eq!(DROPPED_ERR.load(Ordering::SeqCst), dropped_err + 1);
}

struct Watched(OwnedResult);
impl vm::VmScriptValue for Watched {
    fn schema() -> VmResult<vm::HostTypeSchema> {
        <OwnedResult as vm::VmScriptValue>::schema()
    }
    fn into_value(self, context: &mut vm::ScriptValueContext<'_>) -> VmResult<Value> {
        CONVERSIONS.fetch_add(1, Ordering::SeqCst);
        vm::VmScriptValue::into_value(self.0, context)
    }
    fn from_value(vm: &mut Vm, value: Value) -> VmResult<Self> {
        <OwnedResult as vm::VmScriptValue>::from_value(vm, value).map(Self)
    }
}
struct LateFailure(bool);
impl vm::VmScriptValue for LateFailure {
    fn schema() -> VmResult<vm::HostTypeSchema> {
        Ok(vm::HostTypeSchema::Int)
    }
    fn into_value(self, context: &mut vm::ScriptValueContext<'_>) -> VmResult<Value> {
        context.publish_owned(ResourceOwned::new(Success(88)))?;
        if self.0 {
            Err(vm::VmError::HostError(
                "later argument conversion rejected".into(),
            ))
        } else {
            Ok(Value::Bool(false))
        }
    }
    fn from_value(_: &mut Vm, _: Value) -> VmResult<Self> {
        Err(vm::VmError::TypeMismatch("test input only"))
    }
}

#[test]
fn vm_aware_callbacks_reject_foreign_unsubscribed_reset_before_conversion() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut store = callback_store();
    let mut other = callback_store();
    let callback = store
        .vm_script_callback_by_name::<(Watched,), OwnedResult>("echo")
        .unwrap();
    let value = || (Watched(GuestResult::Ok(ResourceOwned::new(Success(5)))),);
    let conversions = CONVERSIONS.load(Ordering::SeqCst);
    let closed = CLOSED.load(Ordering::SeqCst);
    assert!(callback.call_with_vm(&mut other, value()).is_err());
    assert_eq!(CONVERSIONS.load(Ordering::SeqCst), conversions);
    assert_eq!(other.vm_mut().host_context().resource_count(), 0);
    let cloned = callback.clone();
    cloned.unsubscribe();
    assert!(!callback.is_subscribed());
    assert!(callback.call_with_vm(&mut store, value()).is_err());
    let callback = store
        .vm_script_callback_by_name::<(Watched,), OwnedResult>("echo")
        .unwrap();
    store.reset_for_reuse().unwrap();
    assert!(callback.call_with_vm(&mut store, value()).is_err());
    assert_eq!(CONVERSIONS.load(Ordering::SeqCst), conversions);
    assert_eq!(CLOSED.load(Ordering::SeqCst), closed);
    assert_eq!(store.vm_mut().host_context().resource_count(), 0);
}

#[test]
fn vm_aware_callback_rolls_back_every_unpublished_argument_only() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut store = callback_store();
    let existing = store
        .vm_mut()
        .host_context()
        .insert_resource(Failure(99))
        .unwrap()
        .handle();
    let callback = store
        .vm_script_callback_by_name::<(OwnedResult, LateFailure), OwnedResult>("pair")
        .unwrap();
    for conversion_failure in [true, false] {
        let closed = CLOSED.load(Ordering::SeqCst);
        let observations = OBSERVATIONS.load(Ordering::SeqCst);
        assert!(
            callback
                .call_with_vm(
                    &mut store,
                    (
                        GuestResult::Ok(ResourceOwned::new(Success(5))),
                        LateFailure(conversion_failure)
                    )
                )
                .is_err()
        );
        assert_eq!(CLOSED.load(Ordering::SeqCst), closed + 2);
        assert_eq!(OBSERVATIONS.load(Ordering::SeqCst), observations);
        assert_eq!(store.vm_mut().host_context().resource_count(), 1);
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
    }
    store
        .vm_mut()
        .host_context()
        .close_resource::<Failure>(existing, ResourceCloseReason::Requested)
        .unwrap();
    assert_eq!(store.vm_mut().host_context().resource_count(), 0);
}

#[test]
fn vm_aware_post_admission_fatal_before_move_reclaims_only_abandoned() {
    assert_post_admission_abandoned("fail_before_move", false);
}

#[test]
fn vm_aware_post_admission_body_fuel_before_move_reclaims_only_abandoned() {
    assert_post_admission_abandoned("fuel_before_move", true);
}

fn assert_post_admission_abandoned(name: &str, fuel: bool) {
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
            .vm_script_callback_by_name::<(OwnedResult,), OwnedResult>(name)
            .unwrap();
        if fuel {
            store.vm_mut().set_fuel_check_interval(1).unwrap();
            store.vm_mut().set_fuel(30);
        }
        let closed = CLOSED.load(Ordering::SeqCst);
        let dropped_ok = DROPPED_OK.load(Ordering::SeqCst);
        let dropped_err = DROPPED_ERR.load(Ordering::SeqCst);
        let observations = OBSERVATIONS.load(Ordering::SeqCst);
        let value = if branch {
            GuestResult::Ok(ResourceOwned::new(Success(61)))
        } else {
            GuestResult::Err(ResourceOwned::new(Failure(73)))
        };
        let error = callback.call_with_vm(&mut store, (value,)).unwrap_err();
        assert!(
            error.to_string().contains(if fuel {
                "host invocation yielded"
            } else {
                "callback transport failure"
            }),
            "{error}"
        );
        assert_eq!(
            OBSERVATIONS.load(Ordering::SeqCst),
            observations + 1,
            "body must execute beyond admission"
        );
        assert_eq!(OBSERVED_RESOURCE_COUNT.load(Ordering::SeqCst), 2);
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
            "post_admission name={name} branch={branch} live={} close_delta={} drop_ok_delta={} drop_err_delta={}",
            store.vm_mut().host_context().resource_count(),
            CLOSED.load(Ordering::SeqCst) - closed,
            DROPPED_OK.load(Ordering::SeqCst) - dropped_ok,
            DROPPED_ERR.load(Ordering::SeqCst) - dropped_err
        );
        assert_eq!(
            store.vm_mut().host_context().resource_count(),
            1,
            "published owner abandoned before VM destruction"
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
        store
            .vm_mut()
            .host_context()
            .close_resource::<Failure>(existing, ResourceCloseReason::Requested)
            .unwrap();
        assert_eq!(store.vm_mut().host_context().resource_count(), 0);
    }
}

#[test]
fn vm_aware_callback_frame_admission_rejection_rolls_back() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut store = callback_store();
    let callback = store
        .vm_script_callback_by_name::<(OwnedResult,), OwnedResult>("echo")
        .unwrap();
    store.vm_mut().set_fuel_check_interval(1).unwrap();
    store.vm_mut().set_fuel(0);
    let closed = CLOSED.load(Ordering::SeqCst);
    let observations = OBSERVATIONS.load(Ordering::SeqCst);
    assert!(
        callback
            .call_with_vm(
                &mut store,
                (GuestResult::Ok(ResourceOwned::new(Success(5))),)
            )
            .is_err()
    );
    assert_eq!(store.vm_mut().host_context().resource_count(), 0);
    assert_eq!(CLOSED.load(Ordering::SeqCst), closed + 1);
    assert_eq!(OBSERVATIONS.load(Ordering::SeqCst), observations);
}

#[test]
fn vm_aware_owned_callback_take_then_new_return_does_not_close_consumed_argument() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut store = callback_store();
    let callback = store
        .vm_script_callback_by_name::<(OwnedResult,), OwnedResult>("replace")
        .unwrap();
    let closed = CLOSED.load(Ordering::SeqCst);
    let calls = HANDLER_CALLS.load(Ordering::SeqCst);
    let returned = callback
        .call_with_vm(
            &mut store,
            (GuestResult::Ok(ResourceOwned::new(Success(5))),),
        )
        .unwrap();
    assert!(matches!(returned, GuestResult::Err(ref value) if value.as_ref().0 == 29));
    assert_eq!(HANDLER_CALLS.load(Ordering::SeqCst), calls + 1);
    assert_eq!(CLOSED.load(Ordering::SeqCst), closed);
    assert_eq!(store.vm_mut().host_context().resource_count(), 0);
}

#[test]
fn vm_aware_owned_callback_fatal_transport_after_take_is_not_guest_err() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut store = callback_store();
    let callback = store
        .vm_script_callback_by_name::<(OwnedResult,), OwnedResult>("fail")
        .unwrap();
    let closed = CLOSED.load(Ordering::SeqCst);
    let calls = HANDLER_CALLS.load(Ordering::SeqCst);
    let created = CREATED_OK.load(Ordering::SeqCst);
    let error = callback
        .call_with_vm(
            &mut store,
            (GuestResult::Ok(ResourceOwned::new(Success(5))),),
        )
        .unwrap_err();
    assert!(error.to_string().contains("callback transport failure"));
    assert_eq!(HANDLER_CALLS.load(Ordering::SeqCst), calls + 1);
    assert_eq!(CREATED_OK.load(Ordering::SeqCst), created);
    assert_eq!(CLOSED.load(Ordering::SeqCst), closed);
    assert_eq!(store.vm_mut().host_context().resource_count(), 0);
}

#[test]
fn vm_aware_nullable_owned_callback_allocates_only_present_selected_payload() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    type Nullable = GuestResult<Option<ResourceOwned<Success>>, ResourceOwned<Failure>>;
    let mut store = callback_store();
    let callback = store
        .vm_script_callback_by_name::<(Nullable,), Nullable>("nullable")
        .unwrap();
    let returned = callback
        .call_with_vm(&mut store, (GuestResult::Ok(None),))
        .unwrap();
    assert!(matches!(returned, GuestResult::Ok(None)));
    assert_eq!(OBSERVED_RESOURCE_COUNT.load(Ordering::SeqCst), 0);
    for value in [
        GuestResult::Ok(Some(ResourceOwned::new(Success(81)))),
        GuestResult::Err(ResourceOwned::new(Failure(93))),
    ] {
        let returned = callback.call_with_vm(&mut store, (value,)).unwrap();
        assert!(
            matches!(returned, GuestResult::Ok(Some(ref resource)) if resource.as_ref().0 == 81)
                || matches!(returned, GuestResult::Err(ref resource) if resource.as_ref().0 == 93)
        );
        assert_eq!(OBSERVED_RESOURCE_COUNT.load(Ordering::SeqCst), 1);
        assert_eq!(store.vm_mut().host_context().resource_count(), 0);
    }
}

struct EncodedResult(Value);
impl vm::VmScriptValue for EncodedResult {
    fn schema() -> VmResult<vm::HostTypeSchema> {
        <OwnedResult as vm::VmScriptValue>::schema()
    }
    fn into_value(self, _: &mut vm::ScriptValueContext<'_>) -> VmResult<Value> {
        Ok(self.0)
    }
    fn from_value(_: &mut Vm, _: Value) -> VmResult<Self> {
        Err(vm::VmError::TypeMismatch("test input only"))
    }
}

#[test]
fn vm_aware_callback_rejects_wrong_key_arena_stale_and_malformed_before_effects() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for tag in [true, false] {
        let mut store = callback_store();
        let mut foreign = empty_vm();
        let callback = store
            .vm_script_callback_by_name::<(EncodedResult,), OwnedResult>("echo")
            .unwrap();
        let value = generated::functions::make(store.vm_mut(), &[Value::Bool(tag)]).unwrap();
        let foreign_value = generated::functions::make(&mut foreign, &[Value::Bool(tag)]).unwrap();
        let (_, payload) = vm::result::payload(&value).unwrap();
        let wrong = vm::result::carrier(!tag, payload.clone());
        let malformed = Value::map(vec![
            (Value::string("tag"), Value::Int(1)),
            (Value::string("payload"), payload.clone()),
        ]);
        let observations = OBSERVATIONS.load(Ordering::SeqCst);
        for denied in [wrong, foreign_value.clone(), malformed] {
            assert!(
                callback
                    .call_with_vm(&mut store, (EncodedResult(denied),))
                    .is_err()
            );
            assert_eq!(OBSERVATIONS.load(Ordering::SeqCst), observations);
            assert_eq!(store.vm_mut().host_context().resource_count(), 1);
            assert_eq!(foreign.host_context().resource_count(), 1);
        }
        callback
            .call_with_vm(&mut store, (EncodedResult(value.clone()),))
            .unwrap();
        let replacement = generated::functions::make(store.vm_mut(), &[Value::Bool(tag)]).unwrap();
        let observations = OBSERVATIONS.load(Ordering::SeqCst);
        assert!(
            callback
                .call_with_vm(&mut store, (EncodedResult(value),))
                .is_err()
        );
        assert_eq!(OBSERVATIONS.load(Ordering::SeqCst), observations);
        assert_eq!(store.vm_mut().host_context().resource_count(), 1);
        callback
            .call_with_vm(&mut store, (EncodedResult(replacement),))
            .unwrap();
        <OwnedResult as vm::VmScriptValue>::from_value(&mut foreign, foreign_value).unwrap();
        assert_eq!(store.vm_mut().host_context().resource_count(), 0);
        assert_eq!(foreign.host_context().resource_count(), 0);
    }
}

#[test]
fn vm_aware_result_decode_validates_active_key_arena_and_generation() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for tag in [true, false] {
        let mut machine = empty_vm();
        let value = generated::functions::make(&mut machine, &[Value::Bool(tag)]).unwrap();
        let (_, payload) = vm::result::payload(&value).unwrap();
        let wrong = vm::result::carrier(!tag, payload.clone());
        let mut foreign = empty_vm();
        assert!(
            <OwnedResult as vm::VmScriptValue>::from_value(&mut foreign, value.clone()).is_err()
        );
        assert!(<OwnedResult as vm::VmScriptValue>::from_value(&mut machine, wrong).is_err());
        assert_eq!(machine.host_context().resource_count(), 1);
        let decoded =
            <OwnedResult as vm::VmScriptValue>::from_value(&mut machine, value.clone()).unwrap();
        assert!(matches!(decoded, GuestResult::Ok(_)) == tag);
        assert_eq!(machine.host_context().resource_count(), 0);
        let replacement = generated::functions::make(&mut machine, &[Value::Bool(tag)]).unwrap();
        assert!(<OwnedResult as vm::VmScriptValue>::from_value(&mut machine, value).is_err());
        assert_eq!(machine.host_context().resource_count(), 1);
        <OwnedResult as vm::VmScriptValue>::from_value(&mut machine, replacement).unwrap();
        assert_eq!(machine.host_context().resource_count(), 0);
    }
}

#[test]
fn generated_owned_conversion_checks_active_key_arena_generation_before_handler() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for (tag, expected) in [(true, 17), (false, 29)] {
        let mut machine = empty_vm();
        let value = generated::functions::make(&mut machine, &[Value::Bool(tag)]).unwrap();
        assert_eq!(machine.host_context().resource_count(), 1);
        let calls = HANDLER_CALLS.load(Ordering::SeqCst);
        let mut foreign = empty_vm();
        assert!(generated::functions::take(&mut foreign, std::slice::from_ref(&value)).is_err());
        assert_eq!(foreign.host_context().resource_count(), 0);
        assert_eq!(machine.host_context().resource_count(), 1);
        let (_, payload) = vm::result::payload(&value).unwrap();
        let wrong_key = vm::result::carrier(!tag, payload.clone());
        assert!(generated::functions::take(&mut machine, &[wrong_key]).is_err());
        assert_eq!(HANDLER_CALLS.load(Ordering::SeqCst), calls);
        assert_eq!(machine.host_context().resource_count(), 1);
        assert_eq!(
            generated::functions::take(&mut machine, std::slice::from_ref(&value)).unwrap(),
            expected
        );
        assert_eq!(machine.host_context().resource_count(), 0);
        assert!(generated::functions::take(&mut machine, std::slice::from_ref(&value)).is_err());
        let replacement = generated::functions::make(&mut machine, &[Value::Bool(tag)]).unwrap();
        assert!(generated::functions::take(&mut machine, &[value]).is_err());
        assert_eq!(machine.host_context().resource_count(), 1);
        assert_eq!(
            generated::functions::take(&mut machine, &[replacement]).unwrap(),
            expected
        );
        assert_eq!(machine.host_context().resource_count(), 0);
    }
}

#[test]
fn owned_publication_rolls_back_after_insertion_before_publication() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut machine = empty_vm();
    let schema = generated::functions::make_descriptor().schema.return_type;
    let closed = CLOSED.load(Ordering::SeqCst);
    let error = vm::result::publish_owned(
        &mut machine,
        ResourceOwned::new(Success(41)),
        &schema,
        |_| {
            Err(vm::VmError::HostError(
                "assembly denied after insertion".into(),
            ))
        },
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("assembly denied after insertion")
    );
    assert_eq!(machine.host_context().resource_count(), 0);
    assert_eq!(CLOSED.load(Ordering::SeqCst), closed + 1);
    let error = vm::result::publish_owned(
        &mut machine,
        ResourceOwned::new(Success(42)),
        &schema,
        |token| Ok(vm::result::carrier(false, token)),
    )
    .unwrap_err();
    assert!(error.to_string().contains("resource"));
    assert_eq!(machine.host_context().resource_count(), 0);
    assert_eq!(CLOSED.load(Ordering::SeqCst), closed + 2);
}

#[test]
fn generated_result_borrows_preserve_resource_until_take() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for (tag, expected) in [(true, 17), (false, 29)] {
        let mut machine = empty_vm();
        let value = generated::functions::make(&mut machine, &[Value::Bool(tag)]).unwrap();
        assert_eq!(
            generated::functions::borrow(&mut machine, std::slice::from_ref(&value)).unwrap(),
            expected
        );
        assert_eq!(machine.host_context().resource_count(), 1);
        assert_eq!(
            generated::functions::bump(&mut machine, std::slice::from_ref(&value)).unwrap(),
            expected + 1
        );
        assert_eq!(machine.host_context().resource_count(), 1);
        assert_eq!(
            generated::functions::take(&mut machine, &[value]).unwrap(),
            expected + 1
        );
        assert_eq!(machine.host_context().resource_count(), 0);
    }
}

#[test]
fn owned_publication_overbudget_schema_is_rejected_before_insertion() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut machine = empty_vm();
    let mut schema = generated::functions::make_descriptor().schema.return_type;
    for _ in 0..vm::MAX_HOST_SCHEMA_DEPTH {
        schema = vm::HostTypeSchema::Optional(Box::new(schema));
    }
    assert!(
        vm::result::publish_owned(
            &mut machine,
            ResourceOwned::new(Success(43)),
            &schema,
            |token| Ok(vm::result::carrier(true, token))
        )
        .is_err()
    );
    assert_eq!(machine.host_context().resource_count(), 0);
}

#[test]
fn owned_publication_quota_failure_preserves_existing_resources() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut machine = empty_vm();
    let limit = vm::resource::handle::DEFAULT_MAX_RESOURCES;
    for _ in 0..limit {
        machine.host_context().insert_resource(Success(1)).unwrap();
    }
    let error = generated::functions::make(&mut machine, &[Value::Bool(false)]).unwrap_err();
    assert!(error.to_string().contains("capacity"), "{error}");
    assert_eq!(machine.host_context().resource_count(), limit);
    machine
        .execution_scope()
        .begin_close(ResourceCloseReason::Requested)
        .unwrap();
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(machine.execution_scope().poll_close(&mut cx).is_ready());
    assert_eq!(machine.host_context().resource_count(), 0);
}

#[test]
fn generated_owned_result_both_branches_publish_then_take_exactly_once() {
    let _lock = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    CREATED_OK.store(0, Ordering::SeqCst);
    CREATED_ERR.store(0, Ordering::SeqCst);
    for (branch, expected) in [("true", 17), ("false", 29)] {
        let mut machine = machine(&format!(
            "use result_resource; result_resource::take(result_resource::make({branch}));"
        ));
        assert_eq!(machine.run().unwrap(), VmStatus::Halted);
        assert_eq!(machine.stack().last(), Some(&Value::Int(expected)));
        assert_eq!(machine.host_context().resource_count(), 0);
    }
    assert_eq!(CREATED_OK.load(Ordering::SeqCst), 1);
    assert_eq!(CREATED_ERR.load(Ordering::SeqCst), 1);
    let descriptors = descriptors();
    assert_eq!(
        descriptors[1].schema.params[0].passing,
        vm::HostParamPassing::TakeOwned
    );
    assert_eq!(descriptors[0].effects.len(), 2);
    assert_eq!(descriptors[1].effects.len(), 2);
}
