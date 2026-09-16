#![cfg(feature = "runtime")]

//! Runtime behavior of `#[pd_host_function]` hidden host-state parameters.
//!
//! A hidden parameter (`HostStateRef<'_, T>` / `HostStateMut<'_, T>`) is
//! resolved from the VM's generic host-private state table before the host
//! body runs. It must never appear in guest arity, the function schema, the
//! catalog fingerprint, or VMBC — and it must resolve to exactly one per-VM
//! instance that survives VM reuse and stays isolated between VMs.

use pd_host_function::pd_host_function;
use vm::host_api::HostState;
use vm::host_extension::{
    HostFunctionDescriptor, HostModuleDescriptor, HostStateEffect, HostStateMut, HostStateRef,
};
use vm::{
    HostFunctionRegistry, HostImport, Program, Value, ValueType, Vm, VmError, VmResult, VmStatus,
    catalog_import_schemas,
};

/// Per-VM call counter used by the generated hosts below.
#[derive(Debug, Default, PartialEq, Eq)]
struct CallCounter {
    calls: u64,
}

impl HostState for CallCounter {
    const KEY: &'static str = "test.call_counter";

    fn initialize() -> Result<Self, String> {
        Ok(Self::default())
    }
}

/// Shared, read-only view over the same per-VM counter.
#[derive(Debug, Default, PartialEq, Eq)]
struct ObservedCalls {
    last: u64,
}

impl HostState for ObservedCalls {
    const KEY: &'static str = "test.observed_calls";

    fn initialize() -> Result<Self, String> {
        Ok(Self::default())
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

    pub fn borrow_arg<T: FromArg>(args: &[Value], index: usize, label: &str) -> VmResult<T> {
        let value = args
            .get(index)
            .ok_or_else(|| VmError::HostError(format!("missing {label}")))?;
        T::from_arg(value, label)
    }

    pub mod functions {
        use super::*;

        /// Counts calls in per-VM host state and returns the running total.
        #[pd_host_function(name = "demo::counted")]
        fn counted(mut state: HostStateMut<'_, CallCounter>, delta: i64) -> VmResult<i64> {
            state.calls += 1;
            Ok(state.calls as i64 * delta)
        }

        /// Reads per-VM counter state through a shared hidden borrow.
        #[pd_host_function(name = "demo::observe")]
        fn observe(state: HostStateRef<'_, CallCounter>) -> VmResult<i64> {
            Ok(state.calls as i64)
        }

        /// Mutates two different per-VM states in one call.
        #[pd_host_function(name = "demo::bump_and_record")]
        fn bump_and_record(
            mut counter: HostStateMut<'_, CallCounter>,
            mut observed: HostStateMut<'_, ObservedCalls>,
            delta: i64,
        ) -> VmResult<i64> {
            counter.calls += 1;
            observed.last = counter.calls * delta as u64;
            Ok(observed.last as i64)
        }

        /// Requests the same state twice; the second borrow must fail closed.
        #[pd_host_function(name = "demo::double_borrow")]
        fn double_borrow(
            first: HostStateMut<'_, CallCounter>,
            second: HostStateMut<'_, CallCounter>,
        ) -> VmResult<i64> {
            let _ = second;
            Ok(first.calls as i64)
        }
    }
}

fn run_program(vm: &mut Vm) -> i64 {
    match vm.run().expect("host call program must run") {
        VmStatus::Halted => {}
        other => panic!("unexpected status: {other:?}"),
    }
    match vm.stack() {
        [Value::Int(value)] => *value,
        other => panic!("unexpected stack: {other:?}"),
    }
}

fn host_program(descriptor: &HostFunctionDescriptor, delta: i64) -> Program {
    let catalog = HostFunctionDescriptor::collect_catalog(std::slice::from_ref(descriptor))
        .expect("descriptor catalog");
    let import = catalog_import_schemas(&catalog, &descriptor.schema.name)
        .into_iter()
        .next()
        .expect("import schema");
    let mut bytecode = vm::BytecodeBuilder::new();
    bytecode.ldc(0);
    bytecode.call(0, 1);
    bytecode.ret();
    Program::with_imports_and_debug(
        vec![Value::Int(delta)],
        bytecode.finish(),
        vec![HostImport {
            name: descriptor.schema.name.clone(),
            arity: 1,
            return_type: ValueType::Int,
        }],
        None,
    )
    .with_host_import_schemas(vec![import])
    .expect("host import schema metadata")
}

#[test]
fn hidden_state_parameter_is_absent_from_guest_schema_and_effects_are_declared() {
    let descriptor = generated_parent::functions::counted_descriptor();

    assert_eq!(
        descriptor.schema.params.len(),
        1,
        "only the guest parameter is visible: {:?}",
        descriptor.schema.params
    );
    assert_eq!(descriptor.schema.params[0].name, "delta");

    let requirements = descriptor.state_requirements();
    assert_eq!(requirements.len(), 1);
    assert_eq!(requirements[0].key(), "test.call_counter");
    assert!(requirements[0].write);

    let read = generated_parent::functions::observe_descriptor();
    assert!(read.schema.params.is_empty());
    assert!(!read.state_requirements()[0].write);

    let effects: Vec<&HostStateEffect> = descriptor
        .effects
        .iter()
        .filter_map(|effect| effect.host_state())
        .collect();
    assert_eq!(effects.len(), 1, "state effects stay runtime metadata");
}

#[test]
fn hidden_state_resolves_through_the_registry_and_persists_across_calls() {
    let descriptor = generated_parent::functions::counted_descriptor();
    let mut registry = HostFunctionRegistry::empty();
    HostModuleDescriptor::install_descriptors(&mut registry, std::slice::from_ref(&descriptor))
        .expect("state-declaring descriptor installs");

    let mut vm = Vm::new(host_program(&descriptor, 2));
    registry.bind_vm_cached(&mut vm).expect("bind");
    assert_eq!(run_program(&mut vm), 2, "first call: one call * delta 2");

    vm.reset_for_reuse().expect("reset must succeed");
    assert_eq!(
        run_program(&mut vm),
        4,
        "host-private state must survive VM reuse"
    );

    let counter = vm.host_state::<CallCounter>().expect("state exists");
    assert_eq!(counter.calls, 2);
}

#[test]
fn hidden_state_stays_isolated_between_vms() {
    let descriptor = generated_parent::functions::counted_descriptor();
    let mut registry = HostFunctionRegistry::empty();
    HostModuleDescriptor::install_descriptors(&mut registry, std::slice::from_ref(&descriptor))
        .expect("install");

    let mut first = Vm::new(host_program(&descriptor, 3));
    registry.bind_vm_cached(&mut first).expect("bind first");
    assert_eq!(run_program(&mut first), 3);
    first.reset_for_reuse().expect("reset must succeed");
    assert_eq!(run_program(&mut first), 6);
    assert_eq!(
        first
            .host_state::<CallCounter>()
            .expect("first state")
            .calls,
        2
    );

    let mut second = Vm::new(host_program(&descriptor, 3));
    registry.bind_vm_cached(&mut second).expect("bind second");
    assert_eq!(run_program(&mut second), 3, "a second VM starts fresh");
    assert_eq!(
        second
            .host_state::<CallCounter>()
            .expect("second state")
            .calls,
        1
    );
}

#[test]
fn two_distinct_states_are_borrowed_in_one_call() {
    let descriptor = generated_parent::functions::bump_and_record_descriptor();
    let mut registry = HostFunctionRegistry::empty();
    HostModuleDescriptor::install_descriptors(&mut registry, std::slice::from_ref(&descriptor))
        .expect("install");

    let catalog = HostFunctionDescriptor::collect_catalog(std::slice::from_ref(&descriptor))
        .expect("catalog");
    let import = catalog_import_schemas(&catalog, "demo::bump_and_record")
        .into_iter()
        .next()
        .expect("import schema");
    let mut bytecode = vm::BytecodeBuilder::new();
    bytecode.ldc(0);
    bytecode.call(0, 1);
    bytecode.ret();
    let program = Program::with_imports_and_debug(
        vec![Value::Int(4)],
        bytecode.finish(),
        vec![HostImport {
            name: "demo::bump_and_record".to_string(),
            arity: 1,
            return_type: ValueType::Int,
        }],
        None,
    )
    .with_host_import_schemas(vec![import.clone()])
    .expect("schema metadata");

    let mut vm = Vm::new(program);
    registry.bind_vm_cached(&mut vm).expect("bind");
    assert_eq!(run_program(&mut vm), 4);
    assert_eq!(
        vm.host_state::<ObservedCalls>()
            .expect("observed state")
            .last,
        4
    );

    let second_program = {
        let mut bytecode = vm::BytecodeBuilder::new();
        bytecode.ldc(0);
        bytecode.call(0, 1);
        bytecode.ret();
        Program::with_imports_and_debug(
            vec![Value::Int(4)],
            bytecode.finish(),
            vec![HostImport {
                name: "demo::bump_and_record".to_string(),
                arity: 1,
                return_type: ValueType::Int,
            }],
            None,
        )
        .with_host_import_schemas(vec![import])
        .expect("schema metadata")
    };
    let mut second = Vm::new(second_program);
    registry.bind_vm_cached(&mut second).expect("bind second");
    assert_eq!(run_program(&mut second), 4, "the second VM is independent");
}

#[test]
fn conflicting_borrows_of_one_state_fail_closed_through_the_host() {
    let mut vm = Vm::new(Program::new(Vec::new(), vec![vm::OpCode::Ret as u8]));
    let error = generated_parent::functions::double_borrow(&mut vm, &[])
        .expect_err("two mutable borrows of one state must fail closed");
    assert!(
        error.to_string().contains("borrow conflict"),
        "diagnostic must name the borrow conflict: {error}"
    );
    assert!(
        error.to_string().contains("CallCounter"),
        "diagnostic must name the state type: {error}"
    );
    assert!(
        error.to_string().contains("demo::double_borrow"),
        "diagnostic must name the host function: {error}"
    );
}
