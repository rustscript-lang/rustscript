#![cfg(feature = "runtime")]

//! Focused lifecycle tests for the generic per-VM host-private state table.
//!
//! Host-private state is the generic storage primitive a host module uses for
//! per-VM caches, policy, and counters that must never appear in guest arity,
//! `HostFunctionSchema`, or catalog fingerprints. These tests cover the
//! lifecycle independently of any concrete host module (regex is covered
//! separately by the regex module tests and `tests/jit/jit_tests.rs`):
//! lazy/provider initialization, explicit preconfiguration, provider conflict
//! detection, borrow conflicts, reuse persistence, VM isolation, exact-once
//! drop, and deterministic diagnostics.

use std::sync::atomic::{AtomicUsize, Ordering};

use vm::host_api::{HostState, HostStateLifetime, HostStateProvider, HostStateRequirement};
use vm::host_extension::install_host_state_requirements;
use vm::{HostStateRef, OpCode, Program, Vm};

static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

/// State initialized by `Default` (lazy default provider).
#[derive(Debug, Default, PartialEq, Eq)]
struct DemoTuning {
    value: i64,
}

impl HostState for DemoTuning {
    const KEY: &'static str = "test.demo_tuning";

    fn initialize() -> Result<Self, String> {
        Ok(Self::default())
    }
}

/// State initialized by an explicit provider rather than a default value.
#[derive(Debug, PartialEq, Eq)]
struct ProvidedTuning {
    value: i64,
}

impl HostState for ProvidedTuning {
    const KEY: &'static str = "test.provided_tuning";

    fn initialize() -> Result<Self, String> {
        Ok(Self { value: 41 })
    }
}

/// State whose provider always fails.
#[derive(Debug, PartialEq, Eq)]
struct FailingTuning;

impl HostState for FailingTuning {
    const KEY: &'static str = "test.failing_tuning";

    fn initialize() -> Result<Self, String> {
        Err("provider exploded".to_string())
    }
}

/// State that records its exact-once drop.
#[derive(Debug, PartialEq, Eq)]
struct DropTracked {
    value: i64,
}

impl HostState for DropTracked {
    const KEY: &'static str = "test.drop_tracked";

    fn initialize() -> Result<Self, String> {
        Ok(Self { value: 0 })
    }
}

impl Drop for DropTracked {
    fn drop(&mut self) {
        DROP_COUNT.fetch_add(1, Ordering::SeqCst);
    }
}

/// A second concrete type that (incorrectly) claims `DemoTuning`'s key.
#[derive(Debug, PartialEq, Eq)]
struct ImpostorTuning {
    value: i64,
}

impl HostState for ImpostorTuning {
    const KEY: &'static str = "test.demo_tuning";

    fn initialize() -> Result<Self, String> {
        Ok(Self { value: 0 })
    }
}

fn empty_vm() -> Vm {
    Vm::new(Program::new(Vec::new(), vec![OpCode::Ret as u8]))
}

#[test]
fn lazy_default_initialization_happens_on_first_access() {
    let mut vm = empty_vm();

    assert!(
        vm.host_state::<DemoTuning>().is_none(),
        "reading per-VM host state must not initialize it"
    );

    {
        let mut context = vm.host_context();
        assert!(context.is_host_state_empty());
        context
            .ensure_host_state::<DemoTuning>("test::tuning", "state read DemoTuning")
            .expect("provider registration must succeed");
        assert!(
            context
                .ensure_host_state::<DemoTuning>("test::tuning", "state read DemoTuning")
                .is_ok(),
            "repeated resolution stays idempotent"
        );
        let state = context
            .host_state_ref::<DemoTuning>("test::tuning", "state read DemoTuning")
            .expect("lazy default initialization must succeed");
        assert_eq!(state.value, 0);
    }

    {
        let context = vm.host_context();
        let mut state = context
            .host_state_mut::<DemoTuning>("test::tuning", "state write DemoTuning")
            .expect("mutable access must succeed");
        state.value = 7;
    }

    let observed = vm.host_state::<DemoTuning>().expect("state must exist");
    assert_eq!(observed.value, 7);
}

#[test]
fn provider_initialization_runs_instead_of_a_default_value() {
    let mut vm = empty_vm();

    let mut context = vm.host_context();
    context
        .ensure_host_state::<ProvidedTuning>("test::tuning", "state read ProvidedTuning")
        .expect("provider-backed state must resolve");
    let state = context
        .host_state_ref::<ProvidedTuning>("test::tuning", "state read ProvidedTuning")
        .expect("provider-backed state must be readable");
    assert_eq!(state.value, 41);
}

#[test]
fn preconfigured_state_is_never_overwritten_by_the_provider() {
    let mut vm = empty_vm();

    let replaced = vm
        .host_context()
        .set_host_state(ProvidedTuning { value: 99 })
        .expect("preconfiguration must succeed");
    assert!(!replaced, "the first preconfiguration is a fresh insert");

    {
        let mut context = vm.host_context();
        context
            .ensure_host_state::<ProvidedTuning>("test::tuning", "state read ProvidedTuning")
            .expect("preconfigured state must resolve without running the provider");
        let state = context
            .host_state_ref::<ProvidedTuning>("test::tuning", "state read ProvidedTuning")
            .expect("preconfigured state must be readable");
        assert_eq!(
            state.value, 99,
            "an explicit preconfiguration must win over lazy provider initialization"
        );
    }

    let replaced = vm
        .host_context()
        .set_host_state(ProvidedTuning { value: 100 })
        .expect("replacement must succeed");
    assert!(replaced, "replacing existing state must report it");
    assert_eq!(
        vm.host_state::<ProvidedTuning>()
            .expect("state present")
            .value,
        100
    );
}

#[test]
fn state_survives_vm_reuse_and_execution_scope_reset() {
    let mut vm = empty_vm();

    {
        let mut context = vm.host_context();
        context
            .ensure_host_state::<DemoTuning>("test::tuning", "state write DemoTuning")
            .expect("resolution must succeed");
        let mut state = context
            .host_state_mut::<DemoTuning>("test::tuning", "state write DemoTuning")
            .expect("mutable access must succeed");
        state.value = 5;
    }

    vm.reset_for_reuse().expect("reset_for_reuse must succeed");
    assert_eq!(
        vm.host_state::<DemoTuning>()
            .expect("state must survive")
            .value,
        5,
        "host-private state must survive VM reuse"
    );

    vm.run().expect("empty program must run");
    vm.reset_for_reuse().expect("reset_for_reuse must succeed");
    assert_eq!(
        vm.host_state::<DemoTuning>()
            .expect("state must survive")
            .value,
        5
    );
}

#[test]
fn state_is_isolated_between_vms() {
    let mut first = empty_vm();
    let mut second = empty_vm();

    first
        .host_context()
        .set_host_state(DemoTuning { value: 1 })
        .expect("first preconfiguration");
    second
        .host_context()
        .set_host_state(DemoTuning { value: 2 })
        .expect("second preconfiguration");

    assert_eq!(
        first.host_state::<DemoTuning>().expect("first state").value,
        1
    );
    assert_eq!(
        second
            .host_state::<DemoTuning>()
            .expect("second state")
            .value,
        2
    );
    assert!(
        second.host_state::<ProvidedTuning>().is_none(),
        "state must never leak between VMs"
    );
}

#[test]
fn state_drops_exactly_once_when_the_vm_drops() {
    DROP_COUNT.store(0, Ordering::SeqCst);
    {
        let mut vm = empty_vm();
        {
            let mut context = vm.host_context();
            context
                .ensure_host_state::<DropTracked>("test::tuning", "state write DropTracked")
                .expect("resolution must succeed");
            let mut state = context
                .host_state_mut::<DropTracked>("test::tuning", "state write DropTracked")
                .expect("mutable access must succeed");
            state.value = 3;
            assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 0);
        }
        vm.reset_for_reuse().expect("reset_for_reuse must succeed");
        assert_eq!(
            DROP_COUNT.load(Ordering::SeqCst),
            0,
            "reuse must not drop host-private state"
        );
    }
    assert_eq!(
        DROP_COUNT.load(Ordering::SeqCst),
        1,
        "host-private state must drop exactly once with its VM"
    );
}

#[test]
fn removing_state_returns_the_owned_value_once() {
    let mut vm = empty_vm();
    vm.host_context()
        .set_host_state(DemoTuning { value: 11 })
        .expect("preconfiguration");

    assert_eq!(
        vm.host_context().take_host_state::<DemoTuning>(),
        Some(DemoTuning { value: 11 })
    );
    assert_eq!(vm.host_context().take_host_state::<DemoTuning>(), None);
    assert!(vm.host_state::<DemoTuning>().is_none());
}

#[test]
fn a_second_type_claiming_the_same_state_key_is_rejected() {
    let mut vm = empty_vm();
    vm.host_context()
        .set_host_state(DemoTuning { value: 1 })
        .expect("preconfiguration");

    let mut context = vm.host_context();
    let error = context
        .ensure_host_state::<ImpostorTuning>("test::tuning", "state read ImpostorTuning")
        .expect_err("a conflicting state key must fail closed");
    let message = error.to_string();
    assert!(
        message.contains("test.demo_tuning"),
        "diagnostic must name the conflicting state key: {message}"
    );
    assert!(
        message.contains("ImpostorTuning") && message.contains("DemoTuning"),
        "diagnostic must name both conflicting types: {message}"
    );
    assert_eq!(error.namespace(), "host::state");

    let still_valid = context
        .host_state_ref::<DemoTuning>("test::tuning", "state read DemoTuning")
        .expect("the existing state stays usable after a rejected conflict");
    assert_eq!(still_valid.value, 1);
}

#[test]
fn conflicting_requirement_lists_are_rejected_before_mutation() {
    let mut vm = empty_vm();

    let impostor = HostStateRequirement {
        provider: HostStateProvider::of::<ImpostorTuning>(),
        write: false,
    };
    install_host_state_requirements(&mut vm, &[impostor]).expect("the first requirement installs");

    let error = install_host_state_requirements(
        &mut vm,
        &[HostStateRequirement {
            provider: HostStateProvider::of::<DemoTuning>(),
            write: true,
        }],
    )
    .expect_err("a second type claiming the same key must fail closed");
    assert!(
        error.to_string().contains("test.demo_tuning"),
        "diagnostic must name the conflicting key: {error}"
    );

    assert_eq!(
        HostStateLifetime::Vm,
        HostStateProvider::of::<DemoTuning>().lifetime()
    );
    assert_eq!(
        HostStateProvider::of::<DemoTuning>(),
        DemoTuning::provider(),
        "a state type must have exactly one canonical provider"
    );
}

#[test]
fn provider_initialization_failure_is_a_deterministic_host_error() {
    let mut vm = empty_vm();

    let mut context = vm.host_context();
    let error = context
        .ensure_host_state::<FailingTuning>("test::tuning", "state read FailingTuning")
        .expect_err("a failing provider must not panic");
    let message = error.to_string();
    assert!(
        message.contains("test::tuning"),
        "diagnostic must name the host function: {message}"
    );
    assert!(
        message.contains("state read FailingTuning"),
        "diagnostic must name the effect: {message}"
    );
    assert!(
        message.contains("FailingTuning"),
        "diagnostic must name the state type: {message}"
    );
    assert!(
        message.contains("provider exploded"),
        "diagnostic must carry the provider error: {message}"
    );
    assert_eq!(error.namespace(), "host::state");
}

#[test]
fn concurrent_borrows_of_the_same_state_conflict_deterministically() {
    let mut vm = empty_vm();
    vm.host_context()
        .set_host_state(DemoTuning { value: 1 })
        .expect("preconfiguration");

    let context = vm.host_context();
    let shared: HostStateRef<'_, DemoTuning> = context
        .host_state_ref::<DemoTuning>("test::tuning", "state read DemoTuning")
        .expect("shared access must succeed");

    let error = context
        .host_state_mut::<DemoTuning>("test::tuning", "state write DemoTuning")
        .expect_err("a mutable borrow while shared-borrowed must fail closed");
    let message = error.to_string();
    assert!(
        message.contains("borrow conflict") && message.contains("DemoTuning"),
        "diagnostic must name the borrow conflict and type: {message}"
    );
    assert!(
        message.contains("test::tuning"),
        "diagnostic must name the host function: {message}"
    );

    drop(shared);
    let recovered = context
        .host_state_mut::<DemoTuning>("test::tuning", "state write DemoTuning")
        .expect("released borrows must be usable again");
    assert_eq!(recovered.value, 1);
}

#[test]
fn distinct_states_can_be_borrowed_mutably_at_the_same_time() {
    let mut vm = empty_vm();

    let mut context = vm.host_context();
    context
        .ensure_host_state::<DemoTuning>("test::tuning", "state write DemoTuning")
        .expect("first resolution");
    context
        .ensure_host_state::<ProvidedTuning>("test::tuning", "state write ProvidedTuning")
        .expect("second resolution");
    let mut first = context
        .host_state_mut::<DemoTuning>("test::tuning", "state write DemoTuning")
        .expect("first state borrow must succeed");
    let mut second = context
        .host_state_mut::<ProvidedTuning>("test::tuning", "state write ProvidedTuning")
        .expect("second state borrow must succeed");

    first.value = 1;
    second.value = 2;
    assert_eq!((first.value, second.value), (1, 2));
}

#[test]
fn requirement_lists_deduplicate_deterministically() {
    let mut vm = empty_vm();
    let read = HostStateRequirement {
        provider: DemoTuning::provider(),
        write: false,
    };
    let write = HostStateRequirement {
        provider: DemoTuning::provider(),
        write: true,
    };

    let merged = install_host_state_requirements(&mut vm, &[read, write])
        .expect("identical providers must deduplicate");
    assert_eq!(merged.len(), 1, "identical providers must merge");
    assert!(merged[0].write, "a written state is reported as a write");
    assert_eq!(merged[0].key(), "test.demo_tuning");

    install_host_state_requirements(&mut vm, &[read])
        .expect("re-installing the same requirement stays idempotent");
}
