use super::*;
use crate::OwnedHostCall;

#[test]
fn callable_identity_long_lived_factory_metadata_is_bounded() {
    let compiled = crate::compile_source_for_repl(
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
    vm.set_callable_identity_maintenance(CallableIdentityMaintenanceConfig::new(3));
    assert_eq!(vm.run().expect("root execution"), VmStatus::Halted);
    let factory = vm
        .resolve_exported_callable("make_counter")
        .expect("factory");
    let mut checkpoints = Vec::new();
    for round in 1usize..=4096 {
        let value = vm
            .invoke_callable(factory.clone(), &[])
            .expect("factory invocation");
        assert!(matches!(value, Value::Callable(_)));
        drop(value);
        if round.is_power_of_two() {
            checkpoints.push((
                round,
                vm.instance.owned_callables.len(),
                vm.instance.owned_callables.capacity(),
            ));
        }
    }
    println!("weak identity checkpoints: {checkpoints:?}");
    let stats = vm
        .callable_identity_maintenance_stats()
        .expect("enabled policy");
    assert_eq!(stats.registrations, 8192);
    assert_eq!(stats.probes, 3 * stats.registrations);
    assert_eq!(
        stats.entries as u64,
        1 + stats.registrations - stats.retired
    );
    println!("weak identity factory stats: {stats:?}");
    assert!(
        vm.instance.owned_callables.len() <= 32,
        "historical identities: {checkpoints:?}"
    );
    assert!(
        vm.instance.owned_callables.capacity() <= 64,
        "historical vector allocation: {checkpoints:?}"
    );
}

#[test]
fn callable_identity_native_layout_control() {
    let offsets = [
        std::mem::size_of::<Vm>(),
        std::mem::align_of::<Vm>(),
        std::mem::size_of::<Instance>(),
        std::mem::align_of::<Instance>(),
        std::mem::offset_of!(Vm, instance.stack),
        std::mem::offset_of!(Vm, instance.locals),
        std::mem::offset_of!(Vm, instance.ip),
        std::mem::offset_of!(Vm, instance.owned_callables),
        std::mem::offset_of!(Vm, engine.program_constants_ptr),
        std::mem::offset_of!(Vm, run_ctx.fuel_remaining),
        std::mem::offset_of!(Vm, run_ctx.fuel_ops_until_check),
        std::mem::offset_of!(Vm, run_ctx.epoch_deadline),
        std::mem::offset_of!(Vm, run_ctx.epoch_counter_ptr),
        std::mem::offset_of!(Vm, engine.jit_native_region_edge_count),
        std::mem::offset_of!(Vm, engine.jit_native_direct_link_count),
        std::mem::offset_of!(Vm, engine.jit_native_active_direct_trace_id),
    ];
    println!("weak identity native layout: {offsets:?}");
    assert!(offsets.iter().skip(1).all(|offset| *offset < offsets[0]));
}

#[test]
fn callable_identity_root_batches_have_per_identity_budget() {
    for roots in [8usize, 64] {
        let source: String = (0..roots)
            .map(|index| format!("pub fn root_{index}() -> int {{ {index} }}\n"))
            .collect();
        let compiled = crate::compile_source_for_repl(&source).expect("root batch");
        let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
        vm.set_callable_identity_maintenance(CallableIdentityMaintenanceConfig::new(3));
        vm.run().expect("root run");
        let root = vm.resolve_exported_callable("root_0").expect("root");
        for _ in 0..2048 {
            assert_eq!(
                vm.invoke_callable(root.clone(), &[]).expect("invoke"),
                Value::Int(0)
            );
            let stats = vm.callable_identity_maintenance_stats().unwrap();
            assert!(stats.entries <= 4 * roots + 32, "{stats:?}");
            assert!(
                stats.capacity <= (4 * roots + 32).next_power_of_two(),
                "{stats:?}"
            );
        }
        let stats = vm.callable_identity_maintenance_stats().unwrap();
        assert_eq!(stats.registrations, (2048 * roots) as u64);
        assert_eq!(stats.probes, 3 * stats.registrations);
        println!("weak identity root batch roots={roots}: {stats:?}");
    }
}

#[test]
fn callable_identity_repeated_owned_graph_adoption_does_not_duplicate_live_records() {
    let compiled = crate::compile_source_for_repl(
        r#"
        pub fn make_counter() {
            let mut count = 0;
            fn next() { count = count + 1; count }
            next
        }
    "#,
    )
    .expect("source");
    let mut source = Vm::new(compiled.program.with_local_count(compiled.locals));
    source.run().expect("run");
    let factory = source.resolve_exported_callable("make_counter").unwrap();
    let counter = source.invoke_callable(factory, &[]).unwrap();
    let graph = Value::array(vec![counter.clone(); 64]);
    let registry = HostFunctionRegistry::empty();
    let mut call = OwnedHostCall::new(&mut source, Vec::new());
    let mut child = call
        .spawn_owned_callable_vm(&registry, &graph, |vm| {
            vm.set_callable_identity_maintenance(CallableIdentityMaintenanceConfig::new(3));
            Ok(())
        })
        .expect("adoption");
    assert_eq!(
        child
            .callable_identity_maintenance_stats()
            .unwrap()
            .registrations,
        1
    );
    for round in 1..=4096 {
        child
            .recover_owned_callable(&graph)
            .expect("legitimate readoption after reset");
        assert_eq!(
            child
                .invoke_callable(counter.clone(), &[])
                .expect("retained graph"),
            Value::Int(round)
        );
        let stats = child.callable_identity_maintenance_stats().unwrap();
        assert!(stats.entries <= 4, "{stats:?}");
        assert!(stats.capacity <= 8, "{stats:?}");
        let Value::Callable(pointer) = &counter else {
            unreachable!()
        };
        assert_eq!(
            child
                .instance
                .owned_callables
                .iter()
                .filter(|record| record
                    .upgrade()
                    .is_some_and(|owned| Arc::ptr_eq(&owned, pointer)))
                .count(),
            1
        );
    }
    println!(
        "weak identity owned graph: {:?}",
        child.callable_identity_maintenance_stats()
    );
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn callable_identity_native_materialization_prunes_history_without_invalidating_aliases() {
    let compiled = crate::compile_source_for_repl("pub fn answer() -> int { 42 }").unwrap();
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.set_callable_identity_maintenance(CallableIdentityMaintenanceConfig::new(3));
    vm.run().unwrap();
    let prototype = vm.program.root_callable_bindings[0].prototype_id;
    let mut slot = Value::Null;
    let mut aliases = Vec::new();
    // Invoke the existing native ABI entry with its exact declared signature.
    let materialize: extern "C" fn(*mut Vm, *mut Value, i64) -> i32 =
        unsafe { std::mem::transmute(native::materialize_root_callable_entry_address()) };
    for round in 0..16384 {
        assert_eq!(
            materialize(&mut vm, &mut slot, i64::from(prototype)),
            native::STATUS_CONTINUE
        );
        if round < 64 {
            aliases.push(slot.clone());
        }
        let stats = vm.callable_identity_maintenance_stats().unwrap();
        assert!(stats.entries <= 4 * 66 + 32, "{stats:?}");
        assert!(stats.capacity <= 512, "{stats:?}");
    }
    let stats = vm.callable_identity_maintenance_stats().unwrap();
    assert_eq!(stats.registrations, 16384);
    assert_eq!(stats.probes, 3 * stats.registrations);
    for alias in aliases {
        assert_eq!(vm.invoke_callable(alias, &[]).unwrap(), Value::Int(42));
    }
    println!("weak identity native materializations: {stats:?}");
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn callable_identity_real_jit_and_aot_preserve_bounded_registration() {
    let source = r#"
        fn answer() -> int { 42 }
        let function = answer;
        let mut index = 0;
        let mut result = 0;
        while index < 16384 {
            result = function();
            index = index + 1;
        }
        result;
    "#;
    for aot in [false, true] {
        let compiled = crate::compile_source_for_repl(source).unwrap();
        let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
        vm.set_callable_identity_maintenance(CallableIdentityMaintenanceConfig::new(3));
        if aot {
            vm.compile_aot().expect("AOT");
        } else {
            vm.set_jit_config(jit::JitConfig {
                enabled: true,
                hot_loop_threshold: 1,
                max_trace_len: 512,
            });
        }
        assert_eq!(vm.run().unwrap(), VmStatus::Halted);
        assert_eq!(vm.stack(), &[Value::Int(42)]);
        let executions = if aot {
            vm.aot_exec_count()
        } else {
            vm.jit_native_exec_count()
        };
        assert!(executions > 0, "must execute native code");
        let stats = vm.callable_identity_maintenance_stats().unwrap();
        assert!(
            stats.registrations >= 16384,
            "must mint fresh identities: {stats:?}"
        );
        assert_eq!(stats.probes, 3 * stats.registrations);
        assert!(stats.entries <= 32, "{stats:?}");
        assert!(stats.capacity <= 64, "{stats:?}");
        assert!(!vm.engine.aot_interpreter_boundary_hit);
        println!("weak identity backend aot={aot} native_executions={executions}: {stats:?}");
    }
}
