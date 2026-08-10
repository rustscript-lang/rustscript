#![cfg(feature = "runtime")]

use std::sync::{Arc, Mutex};

use vm::{
    EventPayload, EventSink, HostFunctionRegistry, RuntimeResult, Value, Vm, VmStatus,
    compile_source,
};

struct RecordingEventSink(Arc<Mutex<Vec<Value>>>);

impl EventSink for RecordingEventSink {
    fn emit(&mut self, payload: EventPayload) -> RuntimeResult<()> {
        self.0
            .lock()
            .expect("event capture lock should not be poisoned")
            .push(payload.into_value());
        Ok(())
    }
}

#[test]
fn runtime_input_host_reads_embedding_run_value() {
    let program = compile_source(
        r#"
        use runtime;
        runtime::input();
        "#,
    )
    .expect("runtime input source should compile")
    .program;
    let mut vm = Vm::new(program);
    vm.set_runtime_input(Value::string("run-input"))
        .expect("runtime input should be configurable");
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default runtime host registry should bind");

    assert_eq!(
        vm.run().expect("runtime input should execute"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack().last(), Some(&Value::string("run-input")));
}

#[test]
fn runtime_input_host_reports_missing_embedding_value() {
    let program = compile_source(
        r#"
        use runtime;
        runtime::input();
        "#,
    )
    .expect("runtime input source should compile")
    .program;
    let mut vm = Vm::new(program);
    HostFunctionRegistry::new()
        .bind_vm_cached(&mut vm)
        .expect("default runtime host registry should bind");

    let error = vm.run().expect_err("missing runtime input should fail");
    assert!(error.to_string().contains("input_unavailable"));
}

#[test]
fn public_runtime_event_contract_is_implementable_and_configurable() {
    let program = compile_source("0;")
        .expect("minimal runtime host program should compile")
        .program;
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut vm = Vm::new(program);
    vm.set_runtime_event_sink(RecordingEventSink(Arc::clone(&events)))
        .expect("public EventSink implementation should be configurable");
    vm.clear_runtime_event_sink();
}

#[cfg(feature = "sqlite")]
#[test]
fn public_sqlite_policy_configures_the_production_vm() {
    let program = compile_source("0;")
        .expect("minimal SQLite host program should compile")
        .program;
    let mut vm = Vm::new(program);
    vm.configure_sqlite(vm::SqlitePolicy::default());
    let _limits = vm::SqliteLimits::default();
    vm.clear_sqlite_configuration();
}
