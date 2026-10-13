#![cfg(feature = "runtime")]

use std::sync::Arc;
use vm::{
    CallableIdentityMaintenanceConfig, CallableValue, ScriptCallback, Store, Value, Vm, VmError,
    VmStatus, compile_source_for_repl,
};

fn factory_vm() -> Vm {
    let compiled = compile_source_for_repl(
        r#"
            pub fn make_counter() {
                let mut count = 0;
                fn next() { count = count + 1; count }
                next
            }
        "#,
    )
    .expect("factory source");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    assert_eq!(vm.run().expect("root execution"), VmStatus::Halted);
    vm
}

#[test]
fn fixed_live_aliases_callbacks_and_returned_containers_survive_long_history() {
    for live in [1usize, 8, 64] {
        let mut vm = factory_vm();
        vm.set_callable_identity_maintenance(CallableIdentityMaintenanceConfig::new(3));
        let factory = vm
            .resolve_exported_callable("make_counter")
            .expect("factory");
        let aliases: Vec<_> = (0..live)
            .map(|_| vm.invoke_callable(factory.clone(), &[]).expect("counter"))
            .collect();
        let returned_container = Value::array(aliases.clone());
        let mut store = Store::from_vm(vm);
        let callback: ScriptCallback<(), i64> =
            store.script_callback(aliases[0].clone()).expect("callback");
        store
            .enqueue_callback(callback.prepare(()).expect("prepare"))
            .expect("queue");
        let mut checkpoints = Vec::new();
        for round in 1usize..=16384 {
            let transient = store
                .vm_mut()
                .invoke_callable(factory.clone(), &[])
                .expect("churn");
            drop(transient);
            if round.is_power_of_two() {
                let stats = store
                    .vm()
                    .callable_identity_maintenance_stats()
                    .expect("stats");
                checkpoints.push((round, stats.entries, stats.capacity));
                assert_eq!(stats.probes, 3 * stats.registrations);
                assert!(stats.entries <= 4 * live + 32, "live={live}: {stats:?}");
                assert!(stats.capacity <= (4 * live + 32).next_power_of_two());
            }
        }
        assert_eq!(
            store.drain_callbacks().expect("queued live alias"),
            vec![Value::Int(1)]
        );
        assert_eq!(callback.call(&mut store, ()).expect("callback alias"), 2);
        let Value::Array(container) = returned_container else {
            unreachable!()
        };
        for (index, alias) in container.iter().enumerate() {
            assert_eq!(
                store
                    .vm_mut()
                    .invoke_callable(alias.clone(), &[])
                    .expect("container alias"),
                Value::Int(if index == 0 { 3 } else { 1 })
            );
        }
        println!("weak identity public live={live} checkpoints: {checkpoints:?}");
        drop(container);
        drop(aliases);
        drop(callback);
        for _ in 0..1024 {
            drop(
                store
                    .vm_mut()
                    .invoke_callable(factory.clone(), &[])
                    .expect("post-release churn"),
            );
        }
        let stats = store
            .vm()
            .callable_identity_maintenance_stats()
            .expect("final stats");
        assert!(
            stats.entries <= 8,
            "released aliases must retire: {stats:?}"
        );
    }
}

#[test]
fn raw_clones_stay_valid_and_forged_foreign_stale_and_replacement_handles_are_rejected() {
    let mut vm = factory_vm();
    vm.set_callable_identity_maintenance(CallableIdentityMaintenanceConfig::new(3));
    let factory = vm
        .resolve_exported_callable("make_counter")
        .expect("factory");
    let owned = vm.invoke_callable(factory.clone(), &[]).expect("counter");
    let raw_clone = owned.clone();
    let Value::Callable(arc) = &owned else {
        unreachable!()
    };
    let forged = Value::Callable(Arc::new(CallableValue {
        prototype_id: arc.prototype_id,
        kind: arc.kind,
        env: arc.env.clone(),
    }));
    let mut other = factory_vm();
    other.set_callable_identity_maintenance(CallableIdentityMaintenanceConfig::new(3));
    let foreign = other
        .resolve_exported_callable("make_counter")
        .expect("foreign factory");
    for _ in 0..4096 {
        drop(vm.invoke_callable(factory.clone(), &[]).expect("churn"));
    }
    for invalid in [forged, foreign] {
        let ip = vm.ip();
        let stack = vm.stack().to_vec();
        assert!(matches!(
            vm.invoke_callable(invalid, &[]),
            Err(VmError::InvalidFrameState(
                "callable does not belong to this vm"
            ))
        ));
        assert_eq!(vm.ip(), ip);
        assert_eq!(vm.stack(), stack);
    }
    assert_eq!(
        vm.invoke_callable(raw_clone.clone(), &[])
            .expect("raw clone remains live"),
        Value::Int(1)
    );
    vm.reset_for_reuse().expect("reset");
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(matches!(
        vm.poll_reset_for_reuse(&mut cx),
        std::task::Poll::Ready(Ok(()))
    ));
    assert_eq!(vm.run().expect("rerun"), VmStatus::Halted);
    assert!(vm.callable_identity_maintenance_stats().is_some());
    assert!(vm.invoke_callable(raw_clone.clone(), &[]).is_err());
    assert!(vm.invoke_callable(factory, &[]).is_err());
    let fresh = vm
        .resolve_exported_callable("make_counter")
        .expect("fresh factory");
    assert!(matches!(
        vm.invoke_callable(fresh, &[]),
        Ok(Value::Callable(_))
    ));
    let mut store = Store::from_vm(vm);
    let replacement = factory_vm();
    store.replace_vm(replacement);
    assert!(store.vm().callable_identity_maintenance_stats().is_none());
    assert!(store.vm_mut().invoke_callable(raw_clone, &[]).is_err());
}

#[test]
fn policy_is_explicit_reconfigurable_and_scope_independent() {
    assert!(CallableIdentityMaintenanceConfig::new(0).is_none());
    assert!(CallableIdentityMaintenanceConfig::new(1).is_none());
    assert!(CallableIdentityMaintenanceConfig::new(2).is_none());
    assert_eq!(
        CallableIdentityMaintenanceConfig::new(3)
            .unwrap()
            .probes_per_registration(),
        3
    );
    let mut vm = factory_vm();
    assert!(vm.callable_identity_maintenance_stats().is_none());
    assert!(vm.host_context().is_module_state_empty());
    let scope_counts = (
        vm.host_context().resource_count(),
        vm.host_context().operation_count(),
    );
    vm.set_callable_identity_maintenance(CallableIdentityMaintenanceConfig::new(7));
    assert!(!vm.host_context().is_module_state_empty());
    assert_eq!(
        (
            vm.host_context().resource_count(),
            vm.host_context().operation_count()
        ),
        scope_counts
    );
    let factory = vm
        .resolve_exported_callable("make_counter")
        .expect("factory");
    for _ in 0..32 {
        drop(vm.invoke_callable(factory.clone(), &[]).expect("churn"));
    }
    let stats = vm.callable_identity_maintenance_stats().unwrap();
    assert_eq!(stats.registrations, 64);
    assert_eq!(stats.probes, 7 * stats.registrations);
    assert_eq!(
        (
            vm.host_context().resource_count(),
            vm.host_context().operation_count()
        ),
        scope_counts
    );
    vm.set_callable_identity_maintenance(CallableIdentityMaintenanceConfig::new(3));
    assert_eq!(
        vm.callable_identity_maintenance_stats()
            .unwrap()
            .registrations,
        0
    );
    vm.shutdown();
    assert_eq!(vm.callable_identity_maintenance_stats().unwrap().entries, 0);
    vm.set_callable_identity_maintenance(CallableIdentityMaintenanceConfig::new(5));
    assert!(vm.callable_identity_maintenance_stats().is_some());
    vm.set_callable_identity_maintenance(None);
    assert!(vm.callable_identity_maintenance_stats().is_none());
    assert!(vm.host_context().is_module_state_empty());
}

#[test]
fn enabling_retires_existing_backlog_incrementally_and_keeps_capacity_high_water() {
    let mut vm = factory_vm();
    let factory = vm.resolve_exported_callable("make_counter").unwrap();
    for _ in 0..4096 {
        drop(vm.invoke_callable(factory.clone(), &[]).unwrap());
    }
    vm.set_callable_identity_maintenance(CallableIdentityMaintenanceConfig::new(3));
    let initial = vm.callable_identity_maintenance_stats().unwrap();
    assert_eq!(
        initial.entries, 8193,
        "default registration remains append-only"
    );
    assert_eq!(initial.probes, 0, "configuration must not scan");
    drop(vm.invoke_callable(factory.clone(), &[]).unwrap());
    let first = vm.callable_identity_maintenance_stats().unwrap();
    assert_eq!(first.registrations, 2);
    assert_eq!(first.probes, 6);
    assert!(first.retired <= 6);
    for _ in 0..8192 {
        drop(vm.invoke_callable(factory.clone(), &[]).unwrap());
    }
    let final_stats = vm.callable_identity_maintenance_stats().unwrap();
    assert!(final_stats.entries <= 8, "{final_stats:?}");
    assert_eq!(
        final_stats.capacity, initial.capacity,
        "allocation high water is retained"
    );
    println!("weak identity backlog initial={initial:?} final={final_stats:?}");
}

#[test]
fn active_capture_frame_survives_nested_factory_churn() {
    let compiled = compile_source_for_repl(
        r#"
        pub fn make_counter() {
            let mut count = 40;
            fn next() {
                count = count + 1;
                let mut index = 0;
                while index < 4096 {
                    let transient = make_counter();
                    index = index + 1;
                }
                count = count + 1;
                count
            }
            next
        }
    "#,
    )
    .unwrap();
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_callable_identity_maintenance(CallableIdentityMaintenanceConfig::new(3));
    vm.run().unwrap();
    let factory = vm.resolve_exported_callable("make_counter").unwrap();
    let counter = vm.invoke_callable(factory, &[]).unwrap();
    assert_eq!(
        vm.invoke_callable(counter.clone(), &[]).unwrap(),
        Value::Int(42)
    );
    assert_eq!(vm.invoke_callable(counter, &[]).unwrap(), Value::Int(44));
    let stats = vm.callable_identity_maintenance_stats().unwrap();
    assert!(stats.retired > 8192);
    assert!(stats.entries <= 32, "{stats:?}");
    assert_eq!(stats.probes, 3 * stats.registrations);
    println!("weak identity nested active capture: {stats:?}");
}

#[test]
fn active_host_reentry_remains_rejected_before_effects() {
    struct Reentry;
    impl vm::HostFunction for Reentry {
        fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> vm::VmResult<vm::CallOutcome> {
            let callable = vm.resolve_exported_callable("entry")?;
            let before = vm.callable_identity_maintenance_stats().unwrap();
            assert!(matches!(
                vm.invoke_callable(callable, &[]),
                Err(VmError::InvalidFrameState(
                    "host invocation requires a halted VM"
                ))
            ));
            assert_eq!(vm.callable_identity_maintenance_stats().unwrap(), before);
            Ok(vm::CallOutcome::Return(vm::CallReturn::none()))
        }
    }
    let compiled = compile_source_for_repl(
        r#"
        fn pulse();
        pub fn entry() -> int { pulse(); 42 }
    "#,
    )
    .unwrap();
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.register_function(Box::new(Reentry));
    vm.set_callable_identity_maintenance(CallableIdentityMaintenanceConfig::new(3));
    vm.run().unwrap();
    let callable = vm.resolve_exported_callable("entry").unwrap();
    assert_eq!(vm.invoke_callable(callable, &[]).unwrap(), Value::Int(42));
}
