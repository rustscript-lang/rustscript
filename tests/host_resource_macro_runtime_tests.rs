extern crate vm as vm_sdk;

pub mod vm {
    pub use super::vm_sdk::*;
}

use pd_host_function::pd_host_function;
use std::sync::atomic::{AtomicUsize, Ordering};
use vm::resource::{
    CloseProgress, HostResource, ResourceCloseReason, ResourceMut, ResourceOwned, ResourceRef,
};

use vm::{Program, Value, Vm, VmError, VmResult};

use vm::resource;

pub use vm::host_api;

static KEYED_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);

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
struct KeyedCounter(i64);

impl HostResource for KeyedCounter {
    fn resource_type_key() -> Option<vm::ResourceTypeKey> {
        Some(vm::ResourceTypeKey::new("macro.counter").expect("static key"))
    }

    fn begin_close(
        &mut self,
        _reason: ResourceCloseReason,
    ) -> vm::resource::ResourceResult<CloseProgress> {
        Ok(CloseProgress::Ready)
    }
}

mod generated_parent {
    use super::*;

    pub trait FromArg: Sized {
        fn from_arg(value: &Value, label: &str) -> VmResult<Self>;
    }

    impl FromArg for i64 {
        fn from_arg(value: &Value, label: &str) -> VmResult<Self> {
            match value {
                Value::Int(value) => Ok(*value),
                _ => Err(VmError::HostError(format!("expected {label}"))),
            }
        }
    }

    pub fn arg<T: FromArg>(args: &[Value], index: usize, label: &str) -> VmResult<T> {
        args.get(index)
            .ok_or_else(|| VmError::HostError(format!("missing {label}")))
            .and_then(|value| T::from_arg(value, label))
    }

    pub mod functions {
        use super::*;

        /// Read a counter through a shared resource borrow.
        #[pd_host_function(name = "test::borrow_counter")]
        fn borrow_counter(counter: ResourceRef<'_, Counter>) -> VmResult<i64> {
            Ok(counter.get().0)
        }

        /// Increment a counter through a mutable resource borrow.
        #[pd_host_function(name = "test::borrow_mut_counter")]
        fn borrow_mut_counter(mut counter: ResourceMut<'_, Counter>) -> VmResult<i64> {
            counter.get().0 += 1;
            Ok(counter.get().0)
        }

        /// Consume a counter through the public ResourceOwned wrapper.
        #[pd_host_function(name = "test::take_counter")]
        fn take_counter(counter: ResourceOwned<Counter>) -> VmResult<i64> {
            Ok(counter.into_inner().0)
        }

        /// Consume a counter declared as a concrete TakeOwned parameter.
        #[pd_host_function(name = "test::take_counter_concrete")]
        fn take_counter_concrete(
            #[pd_host_param(passing = "take_owned")] counter: Counter,
        ) -> VmResult<i64> {
            Ok(counter.0)
        }
    }

    pub mod keyed_functions {
        use super::*;

        /// Reads a resource whose declaration carries the matching resource key.
        #[pd_host_function(name = "test::keyed_borrow")]
        fn keyed_borrow(
            #[pd_host_resource(passing = "borrow", key = "macro.counter")] counter: ResourceRef<
                '_,
                KeyedCounter,
            >,
        ) -> VmResult<i64> {
            KEYED_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(counter.get().0)
        }

        /// Deliberately advertises a different resource key.
        #[pd_host_function(name = "test::keyed_borrow_mismatch")]
        fn keyed_borrow_mismatch(
            #[pd_host_resource(passing = "borrow", key = "wrong.counter")] counter: ResourceRef<
                '_,
                KeyedCounter,
            >,
        ) -> VmResult<i64> {
            KEYED_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(counter.get().0)
        }

        /// Takes a resource whose declaration carries the matching resource key.
        #[pd_host_function(name = "test::keyed_take")]
        fn keyed_take(
            #[pd_host_resource(passing = "take_owned", key = "macro.counter")]
            counter: ResourceOwned<KeyedCounter>,
        ) -> VmResult<i64> {
            KEYED_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(counter.into_inner().0)
        }

        /// Deliberately advertises a different key on an owned resource path.
        #[pd_host_function(name = "test::keyed_take_mismatch")]
        fn keyed_take_mismatch(
            #[pd_host_resource(passing = "take_owned", key = "wrong.counter")]
            counter: ResourceOwned<KeyedCounter>,
        ) -> VmResult<i64> {
            KEYED_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(counter.into_inner().0)
        }
    }
}

#[test]
fn generated_public_resource_modes_execute_and_take_stales_the_token() {
    let program = Program::new(Vec::new(), vec![vm::OpCode::Ret as u8]);
    let mut vm = Vm::new(program);
    let token = vm
        .host_context()
        .push_resource(Counter(10))
        .expect("resource registration");
    let args = [Value::Int(token.handle().raw() as i64)];

    assert_eq!(
        generated_parent::functions::borrow_counter(&mut vm, &args)
            .expect("shared resource borrow"),
        10
    );

    let mutable_args = args.clone();
    assert_eq!(
        generated_parent::functions::borrow_mut_counter(&mut vm, &mutable_args)
            .expect("mutable resource borrow"),
        11
    );

    assert_eq!(
        generated_parent::functions::take_counter(&mut vm, &args).expect("take-owned resource"),
        11
    );
    let stale = generated_parent::functions::take_counter(&mut vm, &args)
        .expect_err("the token must be stale after take");
    assert!(stale.to_string().contains("already closed") || stale.to_string().contains("stale"));

    let replacement = vm
        .host_context()
        .push_resource(Counter(21))
        .expect("replacement resource registration");
    let replacement_args = [Value::Int(replacement.handle().raw() as i64)];
    assert_eq!(
        generated_parent::functions::take_counter_concrete(&mut vm, &replacement_args)
            .expect("concrete take-owned resource"),
        21
    );
}

#[test]
fn generated_resource_key_is_checked_before_borrow_and_take_logic() {
    KEYED_HANDLER_CALLS.store(0, Ordering::SeqCst);
    let mut vm = Vm::new(Program::new(Vec::new(), vec![vm::OpCode::Ret as u8]));
    let token = vm
        .host_context()
        .push_resource(KeyedCounter(31))
        .expect("keyed resource registration");
    let args = [Value::Int(token.handle().raw() as i64)];

    assert_eq!(
        generated_parent::keyed_functions::keyed_borrow(&mut vm, &args)
            .expect("matching borrow key"),
        31
    );
    assert_eq!(KEYED_HANDLER_CALLS.load(Ordering::SeqCst), 1);

    let mismatch = generated_parent::keyed_functions::keyed_borrow_mismatch(&mut vm, &args)
        .expect_err("mismatched borrow key must fail before the handler");
    assert!(mismatch.to_string().contains("resource type key"));
    assert_eq!(KEYED_HANDLER_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(vm.host_context().resource_count(), 1);

    let take_mismatch = generated_parent::keyed_functions::keyed_take_mismatch(&mut vm, &args)
        .expect_err("mismatched take key must fail before consuming");
    assert!(take_mismatch.to_string().contains("resource type key"));
    assert_eq!(KEYED_HANDLER_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(vm.host_context().resource_count(), 1);

    assert_eq!(
        generated_parent::keyed_functions::keyed_take(&mut vm, &args).expect("matching take key"),
        31
    );
    assert_eq!(KEYED_HANDLER_CALLS.load(Ordering::SeqCst), 2);
    assert_eq!(vm.host_context().resource_count(), 0);
}
