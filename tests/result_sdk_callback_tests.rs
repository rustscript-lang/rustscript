#![cfg(feature = "runtime")]
use vm::{
    GuestResult, IntoScriptValue, ScriptResult, TypeSchema, Value, Vm, VmError, VmResult, VmStatus,
};

#[derive(Clone, Debug, PartialEq)]
struct Packet<T>(T);
impl<T: IntoScriptValue> IntoScriptValue for Packet<T> {
    fn into_script_value(self) -> Value {
        Value::map(vec![(Value::string("value"), self.0.into_script_value())])
    }
    fn schema() -> Option<TypeSchema> {
        Some(TypeSchema::Named("Packet".into(), vec![T::schema()?]))
    }
}
impl<T: ScriptResult> ScriptResult for Packet<T> {
    fn from_value(value: Value) -> VmResult<Self> {
        let Value::Map(values) = value else {
            return Err(VmError::TypeMismatch("Packet"));
        };
        T::from_value(
            values
                .get(&Value::string("value"))
                .cloned()
                .unwrap_or(Value::Null),
        )
        .map(Self)
    }
    fn schema() -> Option<TypeSchema> {
        Some(TypeSchema::Named("Packet".into(), vec![T::schema()?]))
    }
}

#[test]
fn generic_named_optional_arrays_callback_both_variants() {
    let compiled = vm::compile_source("struct Packet<T> { value: T } pub fn echo(value: Result<Packet<int[]?>, string>) -> Result<Packet<int[]?>, string> { value }").unwrap();
    let mut store = vm::Store::from_vm(Vm::new(compiled.program.with_local_count(compiled.locals)));
    store.vm_mut().set_fuel(100_000);
    assert_eq!(store.run().unwrap(), VmStatus::Halted);
    type R = GuestResult<Packet<Option<Vec<i64>>>, String>;
    let callback = store.script_callback_by_name::<(R,), R>("echo").unwrap();
    for value in [
        GuestResult::Ok(Packet(Some(vec![0, 7]))),
        GuestResult::Ok(Packet(None)),
        GuestResult::Err("error".into()),
    ] {
        assert_eq!(callback.call(&mut store, (value.clone(),)).unwrap(), value);
    }
    assert!(
        store
            .script_callback_by_name::<(GuestResult<i64, String>,), R>("echo")
            .is_err()
    );
    assert!(
        store
            .script_callback_by_name::<(R,), GuestResult<i64, String>>("echo")
            .is_err()
    );
    callback.unsubscribe();
    assert!(
        callback
            .call(&mut store, (GuestResult::Err("error".into()),))
            .is_err()
    );
}

#[test]
fn ordinary_result_callback_prepare_queue_start_and_vm_aware_scalar_controls() {
    type R = GuestResult<i64, String>;
    let compiled = vm::compile_source(
        "pub fn echo(value: Result<int, string>) -> Result<int, string> { value }",
    )
    .unwrap();
    let new_store = || {
        let mut store = vm::Store::from_vm(Vm::new(
            compiled.program.clone().with_local_count(compiled.locals),
        ));
        store.vm_mut().set_fuel(100_000);
        store.run().unwrap();
        store
    };
    let mut store = new_store();
    let callback = store.script_callback_by_name::<(R,), R>("echo").unwrap();
    let queued = callback.prepare((GuestResult::Ok(0),)).unwrap();
    store.enqueue_callback(queued).unwrap();
    let results = store.drain_callbacks().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(
        <R as ScriptResult>::from_value(results[0].clone()).unwrap(),
        GuestResult::Ok(0)
    );
    assert_eq!(
        callback
            .start(&mut store, (GuestResult::Err("queued error".into()),))
            .unwrap(),
        VmStatus::Halted
    );
    assert_eq!(
        store.take_callback_result::<R>().unwrap(),
        Some(GuestResult::Err("queued error".into()))
    );
    let vm_callback = store.vm_script_callback_by_name::<(R,), R>("echo").unwrap();
    assert_eq!(
        vm_callback
            .call_with_vm(&mut store, (GuestResult::Ok(7),))
            .unwrap(),
        GuestResult::Ok(7)
    );
    let mut other = new_store();
    let queued = callback.prepare((GuestResult::Ok(2),)).unwrap();
    assert!(other.enqueue_callback(queued).is_err());
    store.reset_for_reuse().unwrap();
    assert!(callback.prepare((GuestResult::Ok(2),)).is_err());
    assert!(
        vm_callback
            .call_with_vm(&mut store, (GuestResult::Ok(2),))
            .is_err()
    );
}

#[test]
fn malformed_result_callback_arguments_rejected_without_poisoning_store() {
    let compiled = vm::compile_source(
        "pub fn echo(value: Result<int, string>) -> Result<int, string> { value }",
    )
    .unwrap();
    let mut store = vm::Store::from_vm(Vm::new(compiled.program.with_local_count(compiled.locals)));
    store.vm_mut().set_fuel(100_000);
    store.run().unwrap();
    let callback = store
        .script_callback_by_name::<(Value,), GuestResult<i64, String>>("echo")
        .unwrap();
    for value in [
        Value::Null,
        Value::map(vec![
            (Value::string("tag"), Value::Int(1)),
            (Value::string("payload"), Value::Int(0)),
        ]),
        Value::map(vec![(Value::string("tag"), Value::Bool(true))]),
        Value::map(vec![
            (Value::string("tag"), Value::Bool(true)),
            (Value::string("payload"), Value::Bool(false)),
        ]),
        Value::map(vec![
            (Value::string("tag"), Value::Bool(true)),
            (Value::string("payload"), Value::Int(0)),
            (Value::string("extra"), Value::Bool(true)),
        ]),
    ] {
        assert!(callback.call(&mut store, (value,)).is_err());
    }
    assert_eq!(
        callback
            .call(&mut store, (vm::result::carrier(true, Value::Int(0)),))
            .unwrap(),
        GuestResult::Ok(0)
    );
}
