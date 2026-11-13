#![cfg(feature = "runtime")]

use pd_host_function::pd_host_function;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use vm::resource::{CloseProgress, HostResource, ResourceCloseReason, ResourceResult};
use vm::{
    FromVmValue, GuestOption, GuestResult, HostNamedStruct, HostResourceType, ResourceOwned,
    ResourceTypeKey, Value, Vm, VmCallable, VmResult, VmStatus,
};

static CREATED: AtomicUsize = AtomicUsize::new(0);
static CLOSED: AtomicUsize = AtomicUsize::new(0);
static OBSERVED_RESOURCE_COUNT: AtomicUsize = AtomicUsize::new(0);
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Clone, Debug, PartialEq)]
struct SdkBox {
    value: i64,
}
impl HostNamedStruct for SdkBox {
    const NAME: &'static str = "OptionSdkBox";
    fn host_struct_fields() -> Vec<vm::HostStructField> {
        vec![vm::HostStructField::new("value", vm::HostTypeSchema::Int)]
    }
}
impl vm::IntoVmValue for SdkBox {
    fn into_vm_value(self) -> Value {
        Value::map(vec![(Value::string("value"), Value::Int(self.value))])
    }
}

#[derive(Debug)]
struct OptionSuccess(i64);
impl HostResource for OptionSuccess {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(ResourceTypeKey::new("option.success").unwrap())
    }
    fn begin_close(&mut self, _: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        CLOSED.fetch_add(1, Ordering::SeqCst);
        Ok(CloseProgress::Ready)
    }
}
impl HostResourceType for OptionSuccess {
    const KEY: &'static str = "option.success";
    const DESCRIPTION: &'static str = "option sdk success resource";
}

#[derive(Debug)]
struct OtherOptionSuccess;
impl HostResource for OtherOptionSuccess {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(ResourceTypeKey::new("option.other").unwrap())
    }
    fn begin_close(&mut self, _: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        Ok(CloseProgress::Ready)
    }
}
impl HostResourceType for OtherOptionSuccess {
    const KEY: &'static str = "option.other";
    const DESCRIPTION: &'static str = "option sdk other resource";
}

mod generated {
    pub use vm::borrow_arg;
    // The host scanner needs the explicit nested callable signatures in this
    // fixture; aliases would hide them from the macro.
    #[allow(clippy::type_complexity)]
    pub mod functions {
        use super::super::*;
        include!("fixtures/option_sdk_signature.rs");
    }
}
include!(concat!(env!("OUT_DIR"), "/option_sdk_build_schemas.rs"));

fn descriptors() -> Vec<vm::HostFunctionDescriptor> {
    vec![
        generated::functions::fixture_descriptor(),
        generated::functions::nested_descriptor(),
        generated::functions::unit_descriptor(),
        generated::functions::inspect_descriptor(),
        generated::functions::option_result_descriptor(),
        generated::functions::result_option_descriptor(),
        generated::functions::callable_echo_descriptor(),
        generated::functions::callable_pick_descriptor(),
        generated::functions::callable_count_descriptor(),
        generated::functions::callable_unit_echo_descriptor(),
        generated::functions::callable_unit_default_echo_descriptor(),
        generated::functions::callable_pair_echo_descriptor(),
        generated::functions::resource_make_descriptor(),
        generated::functions::resource_take_descriptor(),
        generated::functions::resource_observe_descriptor(),
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
    let mut machine = Vm::new(compiled.program.with_local_count(compiled.locals));
    bind_registry(&mut machine);
    machine.set_fuel(100_000);
    machine
}

fn bind_registry(machine: &mut Vm) {
    let descriptors = descriptors();
    let mut registry = vm::HostFunctionRegistry::empty();
    registry.set_capability_profile(vm::CapabilityProfile::deny_all());
    vm::HostModuleDescriptor::install_descriptors(&mut registry, &descriptors).unwrap();
    for descriptor in &descriptors {
        registry.authorize_registered_builtin_import(&descriptor.schema.name);
    }
    registry.bind_vm_cached(machine).unwrap();
}

fn eval(source: &str) -> Value {
    let mut machine = machine(source);
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    machine.stack().last().cloned().expect("value on stack")
}

fn empty_vm() -> Vm {
    Vm::new(vm::Program::new(Vec::new(), vec![vm::OpCode::Ret as u8]))
}

#[test]
fn build_fixture_and_proc_macro_descriptors_are_identical() {
    for (descriptor, (schema, effects, binding)) in [
        (
            generated::functions::fixture_descriptor(),
            result_sdk_build_fixture(),
        ),
        (
            generated::functions::nested_descriptor(),
            result_sdk_build_nested(),
        ),
        (
            generated::functions::unit_descriptor(),
            result_sdk_build_unit(),
        ),
        (
            generated::functions::inspect_descriptor(),
            result_sdk_build_inspect(),
        ),
        (
            generated::functions::option_result_descriptor(),
            result_sdk_build_option_result(),
        ),
        (
            generated::functions::result_option_descriptor(),
            result_sdk_build_result_option(),
        ),
        (
            generated::functions::callable_echo_descriptor(),
            result_sdk_build_callable_echo(),
        ),
        (
            generated::functions::callable_pick_descriptor(),
            result_sdk_build_callable_pick(),
        ),
        (
            generated::functions::callable_count_descriptor(),
            result_sdk_build_callable_count(),
        ),
        (
            generated::functions::callable_unit_echo_descriptor(),
            result_sdk_build_callable_unit_echo(),
        ),
        (
            generated::functions::callable_unit_default_echo_descriptor(),
            result_sdk_build_callable_unit_default_echo(),
        ),
        (
            generated::functions::callable_pair_echo_descriptor(),
            result_sdk_build_callable_pair_echo(),
        ),
        (
            generated::functions::resource_make_descriptor(),
            result_sdk_build_resource_make(),
        ),
        (
            generated::functions::resource_take_descriptor(),
            result_sdk_build_resource_take(),
        ),
        (
            generated::functions::resource_observe_descriptor(),
            result_sdk_build_resource_observe(),
        ),
    ] {
        assert_eq!(schema, descriptor.schema);
        assert_eq!(effects, descriptor.effects);
        assert_eq!(binding, descriptor.binding.kind);
    }
    assert!(matches!(
        generated::functions::fixture_descriptor().schema.params[0].ty,
        vm::HostTypeSchema::Option(_)
    ));
    assert!(matches!(
        generated::functions::unit_descriptor().schema.return_type,
        vm::HostTypeSchema::Unit
    ));
    assert!(matches!(
        generated::functions::resource_take_descriptor()
            .schema
            .params[0]
            .ty,
        vm::HostTypeSchema::Option(_)
    ));
    assert!(matches!(
        generated::functions::resource_make_descriptor()
            .schema
            .return_type,
        vm::HostTypeSchema::Option(_)
    ));
    let callable_schema = &generated::functions::callable_echo_descriptor()
        .schema
        .params[0]
        .ty;
    assert_eq!(
        generated::functions::callable_echo_descriptor()
            .schema
            .params[0]
            .passing,
        vm::HostParamPassing::Value,
        "a callable payload owns no resource and uses value passing"
    );
    let vm::HostTypeSchema::Option(callable) = callable_schema else {
        panic!("callable Option parameter schema: {callable_schema:?}");
    };
    let vm::HostTypeSchema::Callable { params, result } = callable.as_ref() else {
        panic!("callable Option payload must carry the Callable schema");
    };
    assert_eq!(params, &vec![vm::HostTypeSchema::Int]);
    assert!(matches!(result.as_ref(), vm::HostTypeSchema::Int));

    let unit_schema = &generated::functions::callable_unit_echo_descriptor()
        .schema
        .params[0]
        .ty;
    let vm::HostTypeSchema::Option(unit) = unit_schema else {
        panic!("Unit callable Option parameter schema: {unit_schema:?}");
    };
    let vm::HostTypeSchema::Callable { params, result } = unit.as_ref() else {
        panic!("Unit callable payload must carry the Callable schema");
    };
    assert!(params.is_empty());
    assert!(matches!(result.as_ref(), vm::HostTypeSchema::Unit));

    let defaulted_unit_schema = &generated::functions::callable_unit_default_echo_descriptor()
        .schema
        .params[0]
        .ty;
    assert_eq!(
        defaulted_unit_schema, unit_schema,
        "omitted and explicit Unit callable spellings must publish the same schema"
    );
    assert_eq!(
        generated::functions::callable_unit_default_echo_descriptor()
            .schema
            .params[0]
            .passing,
        vm::HostParamPassing::Value
    );

    let pair_schema = &generated::functions::callable_pair_echo_descriptor()
        .schema
        .params[0]
        .ty;
    let vm::HostTypeSchema::Option(pair) = pair_schema else {
        panic!("pair callable Option parameter schema: {pair_schema:?}");
    };
    let vm::HostTypeSchema::Callable { params, result } = pair.as_ref() else {
        panic!("pair callable payload must carry the Callable schema");
    };
    assert_eq!(
        params,
        &vec![vm::HostTypeSchema::Int, vm::HostTypeSchema::Bool]
    );
    assert!(matches!(result.as_ref(), vm::HostTypeSchema::Int));
}

#[test]
fn logical_option_and_result_signatures_do_not_interchange() {
    let catalog = Arc::new(vm::HostFunctionDescriptor::collect_catalog(&descriptors()).unwrap());
    let error = match vm::compile_source_with_flavor_and_options(
        "use option_sdk;\n\
         let value: Result<int[], string> = Result::Ok([3, 4]);\n\
         option_sdk::fixture(value);",
        vm::SourceFlavor::RustScript,
        vm::CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog)),
    ) {
        Ok(_) => panic!("Option parameter must reject an identical-shaped Result carrier"),
        Err(error) => error,
    };
    let message = error.to_string();
    assert!(
        message.contains("Option") && message.contains("Result"),
        "Option/Result signature mismatch was {message}"
    );

    let mut builder = vm::HostApiBuilder::new();
    builder.function(vm::HostFunctionSchema::with_return(
        "mismatch::result",
        vec![vm::HostParamSchema::value(
            "value",
            vm::HostTypeSchema::Result {
                ok: Box::new(vm::HostTypeSchema::Array(Box::new(vm::HostTypeSchema::Int))),
                err: Box::new(vm::HostTypeSchema::String),
            },
        )],
        vm::HostTypeSchema::Int,
    ));
    let result_catalog = Arc::new(builder.build().expect("result catalog"));
    let error = match vm::compile_source_with_flavor_and_options(
        "use mismatch;\n\
         let value: Option<int[]> = Option::Some([3, 4]);\n\
         mismatch::result(value);",
        vm::SourceFlavor::RustScript,
        vm::CompileSourceFileOptions::default().with_host_api_catalog(result_catalog),
    ) {
        Ok(_) => panic!("Result parameter must reject an identical-shaped Option carrier"),
        Err(error) => error,
    };
    let message = error.to_string();
    assert!(
        message.contains("Option") && message.contains("Result"),
        "Result/Option signature mismatch was {message}"
    );
}

#[test]
fn rust_option_roundtrips_through_generated_host_and_rss() {
    for (source, expected) in [
        (
            "use option_sdk; let value: Option<int[]> = option_sdk::fixture(Option::Some([3, 4])); match value { Option::Some(items) => items[0] + items[1], Option::None => -1, };",
            7,
        ),
        (
            "use option_sdk; let value: Option<int[]> = option_sdk::fixture(Option::None); match value { Option::Some(items) => 100, Option::None => -1, };",
            -1,
        ),
    ] {
        let value = eval(source);
        match value {
            Value::Int(value) => assert_eq!(value, expected),
            other => panic!("unexpected value {other:?}"),
        }
    }
    // `Some([])` stays a selected Some with an empty payload; `inspect` proves
    // it is distinct from None without indexing an empty array.
    let value = eval("use option_sdk; option_sdk::inspect(Option::Some([]));");
    assert_eq!(value, Value::Int(0));
}

#[test]
fn nested_option_states_cross_the_generated_host_boundary() {
    for (mode, expected) in [(0, 0), (1, 1), (2, 107)] {
        let value = eval(&format!(
            "use option_sdk; let nested: Option<Option<OptionSdkBox>> = option_sdk::nested({mode}); match nested {{ Option::Some(inner) => match inner {{ Option::Some(boxed) => boxed.value + 100, Option::None => 1 }}, Option::None => 0 }};"
        ));
        assert_eq!(value, Value::Int(expected), "mode {mode}");
    }
}

#[test]
fn unit_host_returns_carry_logical_unit_to_rss() {
    let value = eval("use option_sdk; let unit: () = option_sdk::unit(); unit;");
    assert_eq!(value, Value::Null);
    assert_eq!(
        generated::functions::unit_descriptor().schema.return_type,
        vm::HostTypeSchema::Unit
    );
}

#[test]
fn generated_host_roundtrips_callable_option_payloads() {
    let source = r#"
        use option_sdk;
        pub fn double(value: int) -> int { value + value }
        fn announce() { }
        fn invoke_unit(f: fn() -> ()) -> int { f(); 5 }
        fn pick(value: int, scale: bool) -> int { if scale => { value * 2 } else => { value } }
        let echoed: Option<fn(int) -> int> = option_sdk::callable_echo(Option::Some(double));
        let doubled = match echoed { Option::Some(f) => f(4), Option::None => -1 };
        let absent: Option<fn(int) -> int> = option_sdk::callable_echo(Option::None);
        let skipped = match absent { Option::Some(f) => f(4), Option::None => -2 };
        let picked: Option<fn(int) -> int> = option_sdk::callable_pick(true);
        let from_host = match picked { Option::Some(f) => f(5), Option::None => -3 };
        let empty: Option<fn(int) -> int> = option_sdk::callable_pick(false);
        let missing = match empty { Option::Some(f) => f(5), Option::None => -4 };
        let offset = 10;
        let add: fn(int) -> int = |value| value + offset;
        let carried: Option<fn(int) -> int> = option_sdk::callable_echo(Option::Some(add));
        let captured = match carried { Option::Some(f) => f(6) + f(7), Option::None => -5 };
        let unit_some: Option<fn() -> ()> = option_sdk::callable_unit_echo(Option::Some(announce));
        let unit_result = match unit_some { Option::Some(f) => invoke_unit(f), Option::None => -6 };
        let unit_none: Option<fn() -> ()> = option_sdk::callable_unit_echo(Option::None);
        let unit_skipped = match unit_none { Option::Some(f) => invoke_unit(f), Option::None => -7 };
        let pair: Option<fn(int, bool) -> int> = option_sdk::callable_pair_echo(Option::Some(pick));
        let pair_result = match pair { Option::Some(f) => f(9, true), Option::None => -8 };
        doubled + skipped + from_host + missing + captured + unit_result + unit_skipped + pair_result;
    "#;
    assert_eq!(eval(source), Value::Int(61));

    let none = eval("use option_sdk; option_sdk::callable_count(Option::None);");
    assert_eq!(none, Value::Int(0));
    let some = eval(
        "use option_sdk; fn double(value: int) -> int { value + value } option_sdk::callable_count(Option::Some(double));",
    );
    assert_eq!(some, Value::Int(1));
}

#[test]
fn generated_host_admits_and_invokes_bare_rust_callable_unit() {
    let source = r#"
        use option_sdk;
        fn tick() { let total: int = option_sdk::inspect(Option::Some([2, 3])); }
        fn invoke_unit(f: fn() -> ()) -> int { f(); 5 }
        let selected: Option<fn() -> ()> = option_sdk::callable_unit_default_echo(Option::Some(tick));
        let called = match selected { Option::Some(f) => invoke_unit(f), Option::None => -1 };
        let absent: Option<fn() -> ()> = option_sdk::callable_unit_default_echo(Option::None);
        let skipped = match absent { Option::Some(f) => invoke_unit(f), Option::None => -6 };
        called + skipped;
    "#;
    let mut runtime = machine(source);
    assert_eq!(runtime.run().unwrap(), VmStatus::Halted);
    assert_eq!(runtime.stack().last(), Some(&Value::Int(-1)));
    assert_eq!(
        runtime.host_context().module_state::<usize>().copied(),
        Some(1),
        "Some must invoke the no-tail callable body once; None must stay uncalled"
    );
}

#[test]
fn typed_rust_callback_accepts_returns_and_invokes_guest_option_callable() {
    let compiled = vm::compile_source(
        "fn double(value: int) -> int { value + value }\n\
         fn announce() { }\n\
         fn pick(value: int, scale: bool) -> int { if scale => { value * 2 } else => { value } }\n\
         pub fn make(present: bool) -> Option<fn(int) -> int> { let offset = 1; let add: fn(int) -> int = |value| value + offset; if present => { Option::Some(add) } else => { Option::None } }\n\
         pub fn make_unit(present: bool) -> Option<fn() -> ()> { if present => { Option::Some(announce) } else => { Option::None } }\n\
         pub fn make_pair(present: bool) -> Option<fn(int, bool) -> int> { if present => { Option::Some(pick) } else => { Option::None } }\n\
         pub fn apply(value: Option<fn(int) -> int>) -> int { match value { Option::Some(f) => f(3), Option::None => -1 } }",
    )
    .unwrap();
    let mut store = vm::Store::new(
        Vm::new(compiled.program.with_local_count(compiled.locals)),
        (),
    );
    store.vm_mut().set_fuel(100_000);
    assert_eq!(store.run().unwrap(), VmStatus::Halted);

    let make = store
        .script_callback_by_name::<(bool,), GuestOption<VmCallable<fn(i64) -> i64>>>("make")
        .unwrap();
    let present = make.call(&mut store, (true,)).unwrap();
    let GuestOption::Some(callable) = present else {
        panic!("present selection must carry a typed callable");
    };
    let invoke = store
        .script_callback::<(i64,), i64>(callable.into_value())
        .unwrap();
    assert_eq!(invoke.call(&mut store, (7,)).unwrap(), 8);
    assert_eq!(invoke.call(&mut store, (8,)).unwrap(), 9);
    assert!(matches!(
        make.call(&mut store, (false,)).unwrap(),
        GuestOption::None
    ));

    let apply = store
        .script_callback_by_name::<(GuestOption<VmCallable<fn(i64) -> i64>>,), i64>("apply")
        .unwrap();
    let held = match make.call(&mut store, (true,)).unwrap() {
        GuestOption::Some(value) => value,
        GuestOption::None => panic!("second present selection must carry a callable"),
    };
    assert_eq!(
        apply.call(&mut store, (GuestOption::Some(held),)).unwrap(),
        4
    );
    assert_eq!(apply.call(&mut store, (GuestOption::None,)).unwrap(), -1);

    let make_unit = store
        .script_callback_by_name::<(bool,), GuestOption<VmCallable<fn() -> ()>>>("make_unit")
        .unwrap();
    let unit_selected = match make_unit.call(&mut store, (true,)).unwrap() {
        GuestOption::Some(value) => value,
        GuestOption::None => panic!("present Unit selection must carry a callable"),
    };
    let unit_invoke = store
        .script_callback::<(), ()>(unit_selected.into_value())
        .unwrap();
    assert_eq!(unit_invoke.call(&mut store, ()).unwrap(), ());
    assert!(matches!(
        make_unit.call(&mut store, (false,)).unwrap(),
        GuestOption::None
    ));

    // The omitted Rust spelling is the same public callable type and must
    // admit the no-tail RSS function through the typed Store path too.
    let make_unit_bare = store
        .script_callback_by_name::<(bool,), GuestOption<VmCallable<fn()>>>("make_unit")
        .unwrap();
    let bare_selected = match make_unit_bare.call(&mut store, (true,)).unwrap() {
        GuestOption::Some(value) => value,
        GuestOption::None => panic!("bare Unit selection must carry a callable"),
    };
    let bare_invoke = store
        .script_callback::<(), ()>(bare_selected.into_value())
        .unwrap();
    assert_eq!(bare_invoke.call(&mut store, ()).unwrap(), ());
    assert!(matches!(
        make_unit_bare.call(&mut store, (false,)).unwrap(),
        GuestOption::None
    ));

    let make_pair = store
        .script_callback_by_name::<(bool,), GuestOption<VmCallable<fn(i64, bool) -> i64>>>(
            "make_pair",
        )
        .unwrap();
    let pair_selected = match make_pair.call(&mut store, (true,)).unwrap() {
        GuestOption::Some(value) => value,
        GuestOption::None => panic!("present pair selection must carry a callable"),
    };
    let pair_invoke = store
        .script_callback::<(i64, bool), i64>(pair_selected.into_value())
        .unwrap();
    assert_eq!(pair_invoke.call(&mut store, (4, true)).unwrap(), 8);
    assert_eq!(pair_invoke.call(&mut store, (4, false)).unwrap(), 4);

    let cross = match make.call(&mut store, (true,)).unwrap() {
        GuestOption::Some(value) => value.into_value(),
        GuestOption::None => panic!("third present selection must carry a callable"),
    };
    let mut other = vm::Store::new(
        Vm::new(vm::Program::new(Vec::new(), vec![vm::OpCode::Ret as u8])),
        (),
    );
    assert!(
        other.script_callback::<(i64,), i64>(cross.clone()).is_err(),
        "callable from another store must be rejected"
    );

    store.vm_mut().reset_for_reuse().unwrap();
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(matches!(
        store.vm_mut().poll_reset_for_reuse(&mut context),
        std::task::Poll::Ready(Ok(()))
    ));
    assert!(
        store.script_callback::<(i64,), i64>(cross).is_err(),
        "reset must invalidate a previously returned callable"
    );
}

#[test]
fn callable_option_signature_mismatch_rejects_before_guest_side_effects() {
    let descriptors = descriptors();
    let catalog = Arc::new(vm::HostFunctionDescriptor::collect_catalog(&descriptors).unwrap());
    let error = match vm::compile_source_with_flavor_and_options(
        "use option_sdk;\n\
         fn text(value: string) -> string { value }\n\
         option_sdk::callable_echo(Option::Some(text));",
        vm::SourceFlavor::RustScript,
        vm::CompileSourceFileOptions::default().with_host_api_catalog(catalog),
    ) {
        Ok(_) => panic!("a string callable must not satisfy the int callable schema"),
        Err(error) => error,
    };
    let message = error.to_string();
    assert!(
        message.contains("callable") || message.contains("Option"),
        "callable signature mismatch was {message}"
    );

    let compiled = vm::compile_source(
        "fn text(value: string) -> string { value }\n\
             pub fn text_fn() -> fn(string) -> string { text }",
    )
    .unwrap();
    let mut machine = Vm::new(compiled.program.with_local_count(compiled.locals));
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    let text_fn = machine.resolve_exported_callable("text_fn").unwrap();
    let returned = machine.invoke_callable(text_fn, &[]).unwrap();
    let wrapped =
        <VmCallable<fn(i64) -> i64> as FromVmValue>::from_vm_value(&returned, "callable").unwrap();
    let carrier = vm::option::some_carrier(wrapped.into_value());
    let schema = generated::functions::callable_echo_descriptor()
        .schema
        .params[0]
        .ty
        .clone();
    let error = machine
        .validate_guest_result_value(&carrier, &schema)
        .expect_err("runtime admission must reject the mismatched callable signature");
    assert!(error.to_string().contains("callable"), "{error}");
}

#[test]
fn result_and_option_nest_across_the_host_boundary() {
    let value = eval(
        "use option_sdk; let some: Option<Result<int, string>> = option_sdk::option_result(Option::Some(Result::Ok(5))); match some { Option::Some(result) => match result { Result::Ok(v) => v, Result::Err(e) => -10 }, Option::None => -20 };",
    );
    assert_eq!(value, Value::Int(5));
    let value = eval(
        "use option_sdk; let some: Option<Result<int, string>> = option_sdk::option_result(Option::Some(Result::Err(\"bad\"))); match some { Option::Some(result) => match result { Result::Ok(v) => v, Result::Err(e) => 2 }, Option::None => -20 };",
    );
    assert_eq!(value, Value::Int(2));
    let value = eval(
        "use option_sdk; let absent: Option<Result<int, string>> = option_sdk::option_result(Option::None); match absent { Option::Some(result) => 10, Option::None => 3 };",
    );
    assert_eq!(value, Value::Int(3));
    let value = eval(
        "use option_sdk; let nested: Result<Option<int>, string> = option_sdk::result_option(Result::Ok(Option::Some(6))); match nested { Result::Ok(option) => match option { Option::Some(v) => v, Option::None => -1 }, Result::Err(e) => -2 };",
    );
    assert_eq!(value, Value::Int(6));
    let value = eval(
        "use option_sdk; let nested: Result<Option<int>, string> = option_sdk::result_option(Result::Ok(Option::None)); match nested { Result::Ok(option) => match option { Option::Some(v) => 11, Option::None => 4 }, Result::Err(e) => -2 };",
    );
    assert_eq!(value, Value::Int(4));
}

#[test]
fn inspect_observes_selected_option_without_collapsing_states() {
    let mut machine = machine(
        "use option_sdk; option_sdk::inspect(Option::None); option_sdk::inspect(Option::Some([])); option_sdk::inspect(Option::Some([9]));",
    );
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    assert_eq!(machine.host_context().module_state::<usize>(), Some(&3));
}

#[test]
fn typed_conversions_preserve_some_none_unit_and_empty_containers() {
    use vm::{FromVmValue, IntoVmValue, TakeVmValue};
    let zero = GuestOption::<i64>::Some(0).into_vm_value();
    assert_eq!(
        GuestOption::<i64>::from_vm_value(&zero, "option").unwrap(),
        GuestOption::Some(0)
    );
    let mut zero = zero;
    assert_eq!(
        GuestOption::<i64>::take_vm_value(&mut zero, "option").unwrap(),
        GuestOption::Some(0)
    );
    assert_eq!(zero, Value::Null);
    let none = GuestOption::<i64>::None.into_vm_value();
    assert_eq!(
        GuestOption::<i64>::from_vm_value(&none, "option").unwrap(),
        GuestOption::None
    );
    let false_some = GuestOption::<bool>::Some(false).into_vm_value();
    assert_eq!(
        GuestOption::<bool>::from_vm_value(&false_some, "option").unwrap(),
        GuestOption::Some(false)
    );
    let empty_string = GuestOption::<String>::Some(String::new()).into_vm_value();
    assert_eq!(
        GuestOption::<String>::from_vm_value(&empty_string, "option").unwrap(),
        GuestOption::Some(String::new())
    );
    let empty_array = GuestOption::<Vec<i64>>::Some(Vec::new()).into_vm_value();
    assert_eq!(
        GuestOption::<Vec<i64>>::from_vm_value(&empty_array, "option").unwrap(),
        GuestOption::Some(Vec::new())
    );
    let unit_some = GuestOption::<()>::Some(()).into_vm_value();
    assert_eq!(
        GuestOption::<()>::from_vm_value(&unit_some, "option").unwrap(),
        GuestOption::Some(())
    );
    // A canonical Some(()) writer is allowed to omit the Null-valued payload;
    // only the proven Unit schema may restore the missing entry.
    let unit_tag_only = Value::map(vec![(Value::string("tag"), Value::Bool(true))]);
    assert_eq!(
        GuestOption::<()>::from_vm_value(&unit_tag_only, "option").unwrap(),
        GuestOption::Some(())
    );
    assert!(GuestOption::<i64>::from_vm_value(&unit_tag_only, "option").is_err());
    for value in [
        Value::map(vec![
            (Value::string("tag"), Value::Int(1)),
            (Value::string("payload"), Value::Int(0)),
        ]),
        Value::map(vec![(Value::string("payload"), Value::Int(0))]),
        Value::map(vec![
            (Value::string("tag"), Value::Bool(false)),
            (Value::string("payload"), Value::Int(0)),
        ]),
        Value::map(vec![
            (Value::string("tag"), Value::Bool(true)),
            (Value::string("payload"), Value::Int(0)),
            (Value::string("extra"), Value::Int(1)),
        ]),
    ] {
        assert!(
            GuestOption::<i64>::from_vm_value(&value, "option").is_err(),
            "{value:?}"
        );
    }
}

#[test]
fn ordinary_and_vm_aware_callbacks_preserve_option_and_unit() {
    let compiled = vm::compile_source(
        "pub fn echo(value: Option<int>) -> Option<int> { value }\n\
         pub fn choose(mode: int) -> Option<int> { if mode == 0 => { Option::Some(0) } else => { Option::None } }\n\
         pub fn unit() -> () { () }",
    )
    .unwrap();
    let mut store = vm::Store::new(
        Vm::new(compiled.program.with_local_count(compiled.locals)),
        (),
    );
    store.vm_mut().set_fuel(100_000);
    assert_eq!(store.run().unwrap(), VmStatus::Halted);

    let ordinary = store
        .script_callback_by_name::<(GuestOption<i64>,), GuestOption<i64>>("echo")
        .unwrap();
    for value in [GuestOption::Some(0), GuestOption::None] {
        assert_eq!(ordinary.call(&mut store, (value,)).unwrap(), value);
    }
    let vm_aware = store
        .vm_script_callback_by_name::<(GuestOption<i64>,), GuestOption<i64>>("echo")
        .unwrap();
    for value in [GuestOption::Some(7), GuestOption::None] {
        assert_eq!(vm_aware.call(&mut store, (value,)).unwrap(), value);
    }

    let choose = store
        .script_callback_by_name::<(i64,), GuestOption<i64>>("choose")
        .unwrap();
    for (mode, expected) in [(0, GuestOption::Some(0)), (1, GuestOption::None)] {
        assert_eq!(choose.call(&mut store, (mode,)).unwrap(), expected);
    }

    let ordinary_unit = store.script_callback_by_name::<(), ()>("unit").unwrap();
    assert_eq!(ordinary_unit.call(&mut store, ()).unwrap(), ());
    let vm_unit = store.vm_script_callback_by_name::<(), ()>("unit").unwrap();
    assert_eq!(vm_unit.call(&mut store, ()).unwrap(), ());

    let mut other = vm::Store::new(
        Vm::new(vm::Program::new(Vec::new(), vec![vm::OpCode::Ret as u8])),
        (),
    );
    assert!(ordinary.call(&mut other, (GuestOption::Some(1),)).is_err());
}

#[test]
fn generated_catalog_rejects_malformed_option_carriers_before_handler() {
    let descriptor = generated::functions::inspect_descriptor();
    let descriptors = std::slice::from_ref(&descriptor);
    let catalog = vm::HostFunctionDescriptor::collect_catalog(descriptors).unwrap();
    for value in [
        Value::Null,
        Value::map(vec![
            (Value::string("tag"), Value::Int(1)),
            (Value::string("payload"), Value::array(vec![])),
        ]),
        Value::map(vec![(Value::string("payload"), Value::array(vec![]))]),
        Value::map(vec![
            (Value::string("tag"), Value::Bool(false)),
            (Value::string("payload"), Value::array(vec![])),
        ]),
        Value::map(vec![(Value::string("tag"), Value::Bool(true))]),
        Value::map(vec![
            (Value::string("tag"), Value::Bool(true)),
            (
                Value::string("payload"),
                Value::array(vec![Value::Bool(false)]),
            ),
        ]),
        Value::map(vec![
            (Value::string("tag"), Value::Bool(true)),
            (Value::string("payload"), Value::array(vec![])),
            (Value::string("extra"), Value::Int(1)),
        ]),
    ] {
        let import = vm::catalog_import_schemas(&catalog, "option_sdk::inspect").remove(0);
        let mut code = vm::BytecodeBuilder::new();
        code.ldc(0);
        code.call(0, 1);
        code.ret();
        let program = vm::Program::with_imports_and_debug(
            vec![value],
            code.finish(),
            vec![vm::HostImport {
                name: "option_sdk::inspect".into(),
                arity: 1,
                return_type: vm::ValueType::Int,
            }],
            None,
        )
        .with_host_import_schemas(vec![import])
        .unwrap();
        let mut machine = Vm::new(program);
        let mut registry = vm::HostFunctionRegistry::empty();
        registry.set_capability_profile(vm::CapabilityProfile::deny_all());
        vm::HostModuleDescriptor::install_descriptors(&mut registry, descriptors).unwrap();
        registry.authorize_registered_builtin_import("option_sdk::inspect");
        registry.bind_vm_cached(&mut machine).unwrap();
        machine.set_fuel(100_000);
        assert!(machine.run().is_err());
        assert_eq!(machine.host_context().module_state::<usize>(), None);
        assert_eq!(machine.host_context().resource_count(), 0);
    }
}

#[test]
fn owned_option_resources_publish_take_and_leave_none_owner_free() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut machine = empty_vm();
    let closed = CLOSED.load(Ordering::SeqCst);

    let none = generated::functions::resource_make(&mut machine, &[Value::Bool(false)]).unwrap();
    assert!(matches!(
        none,
        Value::Map(ref map) if matches!(map.get(&Value::string("tag")), Some(Value::Bool(false)))
    ));
    assert_eq!(machine.host_context().resource_count(), 0);
    assert_eq!(
        generated::functions::resource_observe(&mut machine, &[]).unwrap(),
        0
    );
    assert_eq!(
        generated::functions::resource_take(&mut machine, std::slice::from_ref(&none)).unwrap(),
        -1
    );
    assert_eq!(machine.host_context().resource_count(), 0);

    let some = generated::functions::resource_make(&mut machine, &[Value::Bool(true)]).unwrap();
    assert_eq!(machine.host_context().resource_count(), 1);
    assert_eq!(
        generated::functions::resource_observe(&mut machine, &[]).unwrap(),
        1
    );
    assert_eq!(
        generated::functions::resource_take(&mut machine, std::slice::from_ref(&some)).unwrap(),
        23
    );
    assert_eq!(machine.host_context().resource_count(), 0);
    assert!(generated::functions::resource_take(&mut machine, &[some]).is_err());
    assert_eq!(machine.host_context().resource_count(), 0);

    // Cross-VM ownership is rejected; the origin VM keeps its owner.
    let mut origin = empty_vm();
    let mut other = empty_vm();
    let value = generated::functions::resource_make(&mut origin, &[Value::Bool(true)]).unwrap();
    assert_eq!(origin.host_context().resource_count(), 1);
    assert!(generated::functions::resource_take(&mut other, &[value]).is_err());
    assert_eq!(other.host_context().resource_count(), 0);
    assert_eq!(origin.host_context().resource_count(), 1);
    let replacement =
        generated::functions::resource_make(&mut origin, &[Value::Bool(true)]).unwrap();
    generated::functions::resource_take(&mut origin, &[replacement]).unwrap();
    assert_eq!(origin.host_context().resource_count(), 1);
    drop(origin);
    assert!(CLOSED.load(Ordering::SeqCst) > closed);
}

#[test]
fn owned_option_resources_reject_wrong_resource_kind() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut machine = empty_vm();
    let some = generated::functions::resource_make(&mut machine, &[Value::Bool(true)]).unwrap();
    assert_eq!(machine.host_context().resource_count(), 1);
    let error = <GuestOption<ResourceOwned<OtherOptionSuccess>> as vm::VmScriptValue>::from_value(
        &mut machine,
        some.clone(),
    )
    .expect_err("an option.success handle must not decode as option.other");
    assert!(
        error.to_string().contains("resource") || error.to_string().contains("option.other"),
        "wrong option resource kind was {error}"
    );
    let value = generated::functions::resource_take(&mut machine, &[some]).unwrap();
    assert_eq!(value, 23);
    assert_eq!(machine.host_context().resource_count(), 0);
}

#[test]
fn owned_option_publication_rolls_back_after_insertion() {
    let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut machine = empty_vm();
    let schema = generated::functions::resource_make_descriptor()
        .schema
        .return_type;
    let closed = CLOSED.load(Ordering::SeqCst);
    let error = vm::result::publish_owned(
        &mut machine,
        ResourceOwned::new(OptionSuccess(41)),
        &schema,
        |_| Err(vm::VmError::HostError("option assembly denied".into())),
    )
    .unwrap_err();
    assert!(error.to_string().contains("option assembly denied"));
    assert_eq!(machine.host_context().resource_count(), 0);
    assert_eq!(CLOSED.load(Ordering::SeqCst), closed + 1);

    let error = vm::result::publish_owned(
        &mut machine,
        ResourceOwned::new(OptionSuccess(42)),
        &schema,
        |_| Ok(vm::result::carrier(true, Value::Bool(true))),
    )
    .unwrap_err();
    assert!(error.to_string().contains("resource"));
    assert_eq!(machine.host_context().resource_count(), 0);
    assert_eq!(CLOSED.load(Ordering::SeqCst), closed + 2);
}

#[test]
fn rss_constructs_and_transfers_owned_option_resources() {
    let source = r#"
        use option_sdk;
        let made: Option<resource<option.success>> = option_sdk::resource_make(true);
        let before = option_sdk::resource_observe();
        let taken = match made {
            Option::Some(owner) => option_sdk::resource_take(Option::Some(owner)),
            Option::None => -1,
        };
        let after = option_sdk::resource_observe();
        let absent: Option<resource<option.success>> = option_sdk::resource_make(false);
        let none_take = option_sdk::resource_take(absent);
        before + after + taken + none_take;
    "#;
    let mut machine = machine(source);
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    assert_eq!(machine.stack().last(), Some(&Value::Int(23)));
    assert_eq!(machine.host_context().resource_count(), 0);
    assert_eq!(OBSERVED_RESOURCE_COUNT.load(Ordering::SeqCst), 0);
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn option_roundtrip_executes_in_native_aot() {
    let mut machine = machine(
        "use option_sdk; let value: Option<int[]> = option_sdk::fixture(Option::Some([3, 4])); option_sdk::inspect(value);",
    );
    machine.compile_aot().unwrap();
    let dump = machine.dump_aot_info();
    assert!(dump.contains("lowering=ssa"), "{dump}");
    assert!(!dump.contains("interpreter-boundary"), "{dump}");
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    assert_eq!(machine.stack().last(), Some(&Value::Int(7)));
    assert!(machine.aot_exec_count() > 0, "{dump}");
}

#[test]
fn option_roundtrip_executes_from_loaded_vmbc() {
    let source = "use option_sdk; fn double(value: int) -> int { value + value } let value: Option<int[]> = option_sdk::fixture(Option::Some([3, 4])); let callable: Option<fn(int) -> int> = option_sdk::callable_echo(Option::Some(double)); let applied = match callable { Option::Some(f) => f(4), Option::None => -1 }; match value { Option::Some(items) => items[0] + items[1] + applied, Option::None => -1 };";
    let descriptors = descriptors();
    let catalog = Arc::new(vm::HostFunctionDescriptor::collect_catalog(&descriptors).unwrap());
    let compiled = vm::compile_source_with_flavor_and_options(
        source,
        vm::SourceFlavor::RustScript,
        vm::CompileSourceFileOptions::default().with_host_api_catalog(catalog),
    )
    .unwrap();
    let bytes = vm::encode_program(&compiled.program.with_local_count(compiled.locals)).unwrap();
    let decoded = vm::decode_program(&bytes).unwrap();
    let mut loaded = Vm::new(decoded.with_local_count(compiled.locals));
    bind_registry(&mut loaded);
    loaded.set_fuel(100_000);
    assert_eq!(loaded.run().unwrap(), VmStatus::Halted);
    assert_eq!(loaded.stack().last(), Some(&Value::Int(15)));
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn option_states_execute_in_trace_jit() {
    let source = r#"
        fn choose(index: int) -> Option<int> {
            if index == 64 => { Option::None } else => { Option::Some(index) }
        }
        let mut index = 0;
        let mut total = 0;
        while index < 128 {
            let value: Option<int> = choose(index);
            let selected = match value { Option::Some(inner) => inner, Option::None => 1 };
            total = total + selected;
            index = index + 1;
        }
        total;
    "#;
    let mut machine = machine(source);
    machine.set_jit_config(vm::JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    assert_eq!(machine.stack().last(), Some(&Value::Int(8065)));
    assert!(
        machine.jit_native_exec_count() > 0,
        "Option states must execute in a native trace: {}",
        machine.dump_jit_info()
    );
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn option_combinators_and_projection_execute_in_trace_jit() {
    let source = r#"
        struct Row { value: int }
        let row: Row = { value: 2 };
        let mut index = 0;
        let mut total = 0;
        while index < 128 {
            let base: Option<int> = Option::Some(index);
            let mapped = base.map(|value| value + 1).and_then(|value| Option::Some(value + 1));
            let selected = mapped.unwrap_or(0);
            let projected = row?.value.unwrap_or(0);
            total = total + selected + projected;
            index = index + 1;
        }
        total;
    "#;
    let mut machine = machine(source);
    machine.set_jit_config(vm::JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 1024,
    });
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    assert_eq!(machine.stack().last(), Some(&Value::Int(8640)));
    assert!(
        machine.jit_native_exec_count() > 0,
        "Option combinators and projection must execute in a native trace: {}",
        machine.dump_jit_info()
    );
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn option_roundtrip_executes_from_loaded_aot_artifact() {
    let source = "use option_sdk; let value: Option<int[]> = option_sdk::fixture(Option::Some([3, 4])); option_sdk::inspect(value);";
    let mut source_machine = machine(source);
    source_machine.compile_aot().unwrap();
    let artifact = source_machine.encode_aot_artifact().unwrap();
    let program = source_machine.program().clone();
    let mut loaded = Vm::new(program);
    bind_registry(&mut loaded);
    loaded.set_fuel(100_000);
    loaded.load_aot_artifact(&artifact).unwrap();
    assert_eq!(loaded.run().unwrap(), VmStatus::Halted);
    assert_eq!(loaded.stack().last(), Some(&Value::Int(7)));
    assert!(
        loaded.aot_exec_count() > 0,
        "loaded AOT execution must use the native artifact"
    );
}
