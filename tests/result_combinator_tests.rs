#![cfg(feature = "runtime")]

use std::sync::{Arc, Mutex};

use pd_host_function::pd_host_function;
use vm::{
    CompileSourceFileOptions, HostFunctionDescriptor, HostFunctionRegistry, HostModuleDescriptor,
    SourceFlavor, Value, Vm, VmResult, VmStatus, compile_source,
};

fn eval(source: &str) -> Value {
    let compiled = compile_source(source).expect("Result combinator source should compile");
    let mut machine = Vm::new(compiled.program.with_local_count(compiled.locals));
    machine.set_fuel(100_000);
    assert_eq!(
        machine.run().expect("Result combinator program should run"),
        VmStatus::Halted
    );
    machine
        .stack()
        .last()
        .cloned()
        .expect("Result combinator program should leave a value")
}

fn reject(source: &str) -> String {
    match compile_source(source) {
        Ok(_) => panic!("Result combinator source should be rejected: {source}"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn map_transforms_ok_payload_and_passes_err_payload_through() {
    let source = r#"
        let ok_value: Result<int, int> = Result::Ok(2);
        let err_value: Result<int, int> = Result::Err(9);
        let ok_mapped = ok_value.map(|value| value + 1);
        let err_mapped = err_value.map(|value| value + 1);
        let ok_payload = match ok_mapped {
            Result::Ok(value) => value,
            Result::Err(error) => -1,
        };
        let err_payload = match err_mapped {
            Result::Ok(value) => -1,
            Result::Err(error) => error,
        };
        let ok_flag = ok_mapped.is_ok();
        let err_flag = err_mapped.is_err();
        let observed = if ok_flag && err_flag => { 1 } else => { 0 };
        ok_payload + err_payload + observed;
    "#;
    // Ok(2) -> Ok(3); Err(9) stays Err(9); both tag checks hold.
    assert_eq!(eval(source), Value::Int(13));
}

#[test]
fn map_rejects_non_result_receivers() {
    let diagnostic = reject("let value: int = 1; let mapped = value.map(|item| item + 1);");
    assert!(
        diagnostic.contains("Result"),
        "map on a scalar must report the Result receiver requirement: {diagnostic}"
    );
}

#[test]
fn map_err_converts_the_error_branch_and_passes_ok_payload_through() {
    let source = r#"
        fn map_number(error: int) -> string {
            "converted"
        }
        let ok_value: Result<int, int> = Result::Ok(4);
        let err_value: Result<int, int> = Result::Err(6);
        let ok_mapped = ok_value.map_err(map_number);
        let err_mapped = err_value.map_err(map_number);
        let ok_payload = match ok_mapped {
            Result::Ok(value) => value,
            Result::Err(error) => -1,
        };
        let err_observed = match err_mapped {
            Result::Ok(value) => "none",
            Result::Err(error) => error,
        };
        let ok_flag = ok_mapped.is_ok();
        let err_flag = err_mapped.is_err();
        let observed = if ok_flag && err_flag && err_observed == "converted" => { 1 } else => { 0 };
        ok_payload + observed;
    "#;
    assert_eq!(eval(source), Value::Int(5));
}

#[test]
fn map_err_conversion_feeds_question_mark_with_matching_error_type() {
    let source = r#"
        fn load() -> Result<int, string> {
            Result::Err("bad")
        }
        fn convert(error: string) -> int {
            7
        }
        fn relay() -> Result<int, int> {
            let value = load().map_err(convert)?;
            Result::Ok(value)
        }
        let result = relay();
        let observed = match result {
            Result::Ok(value) => value,
            Result::Err(error) => error,
        };
        observed;
    "#;
    assert_eq!(eval(source), Value::Int(7));
}

#[test]
fn map_err_requires_the_closure_error_type_to_match_the_question_mark_error() {
    let diagnostic = reject(
        r#"
        fn load() -> Result<int, string> {
            Result::Err("bad")
        }
        fn convert(error: string) -> int {
            7
        }
        fn relay() -> Result<int, string> {
            load().map_err(convert)?
        }
        relay();
        "#,
    );
    assert!(
        diagnostic.contains("error") || diagnostic.contains("map_err"),
        "mismatched mapped error type must be rejected: {diagnostic}"
    );
}

#[test]
fn and_then_uses_the_selected_branch_result() {
    let source = r#"
        fn continue_with(value: int) -> Result<int, int> {
            if value > 0 => { Result::Ok(value * 2) } else => { Result::Err(-3) }
        }
        let ok_value: Result<int, int> = Result::Ok(5);
        let err_value: Result<int, int> = Result::Err(9);
        let ok_chained = ok_value.and_then(continue_with);
        let err_chained = err_value.and_then(continue_with);
        let ok_payload = match ok_chained {
            Result::Ok(value) => value,
            Result::Err(error) => -1,
        };
        let err_payload = match err_chained {
            Result::Ok(value) => -1,
            Result::Err(error) => error,
        };
        ok_payload + err_payload;
    "#;
    assert_eq!(eval(source), Value::Int(19));
}

#[test]
fn and_then_rejects_a_callback_error_type_mismatch() {
    let diagnostic = reject(
        r#"
        fn continue_with(value: int) -> Result<int, string> {
            Result::Ok(value)
        }
        let value: Result<int, int> = Result::Ok(1);
        let chained = value.and_then(continue_with);
        chained;
        "#,
    );
    assert!(
        diagnostic.contains("and_then"),
        "and_then must keep the receiver error type: {diagnostic}"
    );
}

#[test]
fn or_else_recovers_the_error_branch_and_keeps_the_ok_branch() {
    let source = r#"
        fn recover(error: int) -> Result<int, string> {
            Result::Ok(error + 10)
        }
        let ok_value: Result<int, int> = Result::Ok(3);
        let err_value: Result<int, int> = Result::Err(8);
        let ok_chained: Result<int, string> = ok_value.or_else(recover);
        let err_chained: Result<int, string> = err_value.or_else(recover);
        let ok_payload = match ok_chained {
            Result::Ok(value) => value,
            Result::Err(error) => -1,
        };
        let err_payload = match err_chained {
            Result::Ok(value) => value,
            Result::Err(error) => -1,
        };
        ok_payload + err_payload;
    "#;
    assert_eq!(eval(source), Value::Int(21));
}

#[test]
fn or_else_rejects_a_callback_success_type_mismatch() {
    let diagnostic = reject(
        r#"
        fn recover(error: int) -> Result<string, string> {
            Result::Ok("no")
        }
        let value: Result<int, int> = Result::Err(1);
        let recovered = value.or_else(recover);
        recovered;
        "#,
    );
    assert!(
        diagnostic.contains("or_else"),
        "or_else must keep the receiver success type: {diagnostic}"
    );
}

#[test]
fn unwrap_or_returns_the_selected_payload_or_eager_fallback() {
    let source = r#"
        let ok_value: Result<int, int> = Result::Ok(11);
        let err_value: Result<int, int> = Result::Err(2);
        let ok_out = ok_value.unwrap_or(20);
        let err_out = err_value.unwrap_or(30);
        ok_out + err_out;
    "#;
    assert_eq!(eval(source), Value::Int(41));
}

#[test]
fn unwrap_or_fallback_is_eagerly_evaluated_even_when_ok_is_selected() {
    let source = r#"
        fn fail() -> Result<int, int> {
            Result::Err(7)
        }
        fn relay(value: Result<int, int>) -> Result<int, int> {
            let out = value.unwrap_or(fail()?);
            Result::Ok(out)
        }
        let result = relay(Result::Ok(3));
        let observed = match result {
            Result::Ok(value) => -value,
            Result::Err(error) => error,
        };
        observed;
    "#;
    // The eager fallback `fail()?` exits with Err(7) even though the receiver
    // is Ok(3): a lazy fallback would have produced Ok(3).
    assert_eq!(eval(source), Value::Int(7));
}

#[test]
fn unwrap_or_else_calls_only_the_err_branch_callback_body() {
    let source = r#"
        fn relay(value: Result<int, int>) -> Result<int, int> {
            let out = value.unwrap_or_else(|error| error + 100);
            Result::Ok(out)
        }
        let ok_result = relay(Result::Ok(9));
        let err_result = relay(Result::Err(1));
        let ok_payload = match ok_result {
            Result::Ok(value) => value,
            Result::Err(error) => -1,
        };
        let err_payload = match err_result {
            Result::Ok(value) => value,
            Result::Err(error) => -1,
        };
        ok_payload + err_payload;
    "#;
    // Ok(9) keeps the payload; Err(1) runs the lazy fallback body.
    assert_eq!(eval(source), Value::Int(110));
}

#[test]
fn unwrap_or_else_rejects_a_callback_return_mismatch() {
    let diagnostic = reject(
        r#"
        fn recover(error: int) -> string {
            "no"
        }
        let value: Result<int, int> = Result::Err(1);
        let out = value.unwrap_or_else(recover);
        out;
        "#,
    );
    assert!(
        diagnostic.contains("unwrap_or_else"),
        "unwrap_or_else must require the payload type: {diagnostic}"
    );
}

#[test]
fn combinators_chain_left_to_right_across_both_error_channels() {
    let source = r#"
        fn build(value: bool) -> Result<int, string> {
            if value => { Result::Ok(2) } else => { Result::Err("bad") }
        }
        fn relay(ok: bool) -> Result<int, int> {
            let value = build(ok);
            let final_result: Result<int, int> = value
                .map(|item| item + 1)
                .map_err(|error| 8)
                .and_then(|item| Result::Ok(item * 3))
                .or_else(|error| Result::Ok(error - 1));
            final_result
        }
        let ok_result = relay(true);
        let err_result = relay(false);
        let ok_payload = match ok_result {
            Result::Ok(value) => value,
            Result::Err(error) => -1,
        };
        let err_payload = match err_result {
            Result::Ok(value) => value,
            Result::Err(error) => -1,
        };
        ok_payload + err_payload;
    "#;
    // Ok path: 2 -> 3 -> 9. Err path: "bad" -> 8 -> recovered 7.
    assert_eq!(eval(source), Value::Int(16));
}

#[test]
fn callable_alias_and_generic_receivers_keep_their_schemas() {
    let source = r#"
        fn add_one(value: int) -> int {
            value + 1
        }
        fn wrap<T>(value: T) -> Result<T, int> {
            Result::Ok(value)
        }
        fn apply_identity<T>(value: Result<T, int>) -> Result<T, int> {
            let mapped: Result<T, int> = value.and_then(|item| Result::Ok(item));
            mapped
        }
        let alias = add_one;
        let number: Result<int, int> = Result::Ok(4);
        let incremented = number.map(alias);
        let text_result = apply_identity::<string>(wrap::<string>("hi"));
        let number_payload = match incremented {
            Result::Ok(value) => value,
            Result::Err(error) => -1,
        };
        let text_ok = match text_result {
            Result::Ok(value) => value == "hi",
            Result::Err(error) => false,
        };
        let observed = if text_ok => { 1 } else => { 0 };
        number_payload + observed;
    "#;
    assert_eq!(eval(source), Value::Int(6));
}

#[test]
fn named_payload_and_named_error_keep_their_declarations() {
    let source = r#"
        struct Payload { value: int }
        struct Failure { code: int }
        let value: Result<Payload, Failure> = Result::Ok({ value: 3 });
        let mapped = value.map(|item| item);
        let payload = match mapped {
            Result::Ok(item) => item.value,
            Result::Err(error) => -1,
        };
        let failed: Result<Payload, Failure> = Result::Err({ code: 12 });
        let converted = failed.map_err(|error| error.code);
        let code = match converted {
            Result::Ok(item) => -1,
            Result::Err(error) => error,
        };
        payload + code;
    "#;
    assert_eq!(eval(source), Value::Int(15));
}

#[test]
fn nullable_unwrap_or_keeps_the_existing_nullable_semantics() {
    let source = r#"
        let present: int? = 3;
        let missing: int? = null;
        let present_out = present.unwrap_or(9);
        let missing_out = missing.unwrap_or(8);
        present_out + missing_out;
    "#;
    assert_eq!(eval(source), Value::Int(11));
}

#[test]
fn plain_member_named_map_stays_a_field_read() {
    let source = r#"
        let record = { "map": 5 };
        let value = record.map;
        value;
    "#;
    assert_eq!(eval(source), Value::Int(5));
}

#[test]
fn map_rejects_a_non_callable_argument() {
    let diagnostic = reject(
        r#"
        let value: Result<int, int> = Result::Ok(1);
        let mapped = value.map(7);
        mapped;
        "#,
    );
    assert!(
        diagnostic.contains("callable"),
        "map must require a callable callback: {diagnostic}"
    );
}

#[test]
fn unwrap_or_rejects_a_fallback_type_mismatch() {
    let diagnostic = reject(
        r#"
        let value: Result<int, int> = Result::Ok(1);
        let out = value.unwrap_or("no");
        out;
        "#,
    );
    assert!(
        diagnostic.contains("unwrap_or"),
        "unwrap_or must require the payload type: {diagnostic}"
    );
}

#[test]
fn map_callback_captures_outer_local_by_value() {
    let source = r#"
        let bonus = 3;
        let value: Result<int, int> = Result::Ok(4);
        let mapped = value.map(|item| item + bonus);
        let out = match mapped {
            Result::Ok(item) => item,
            Result::Err(error) => -1,
        };
        out;
    "#;
    assert_eq!(eval(source), Value::Int(7));
}

#[test]
fn map_rejects_a_callback_parameter_type_mismatch() {
    let diagnostic = reject(
        r#"
        fn takes_text(value: string) -> string {
            value
        }
        let value: Result<int, int> = Result::Ok(1);
        let mapped = value.map(takes_text);
        mapped;
        "#,
    );
    assert!(
        diagnostic.contains("map"),
        "map must require the payload parameter type: {diagnostic}"
    );
}

mod probe {
    pub use vm::borrow_arg;
    pub mod functions {
        use super::super::*;

        static EVENTS: Mutex<Vec<i64>> = Mutex::new(Vec::new());

        pub fn clear() {
            EVENTS.lock().expect("probe log").clear();
        }

        pub fn events() -> Vec<i64> {
            EVENTS.lock().expect("probe log").clone()
        }

        fn record(tag: i64) {
            EVENTS.lock().expect("probe log").push(tag);
        }

        /// Appends a probe event and returns the tag.
        #[pd_host_function(name = "probe::mark")]
        fn mark(tag: i64) -> i64 {
            record(tag);
            tag
        }

        /// Appends a probe event and publishes an Ok carrier.
        #[pd_host_function(name = "probe::ok")]
        fn ok(tag: i64) -> vm::VmResult<vm::GuestResult<i64, i64>> {
            record(tag);
            Ok(vm::GuestResult::Ok(tag))
        }

        /// Appends a probe event and publishes an Err carrier.
        #[pd_host_function(name = "probe::err")]
        fn err(tag: i64) -> vm::VmResult<vm::GuestResult<i64, i64>> {
            record(tag);
            Ok(vm::GuestResult::Err(tag))
        }
    }
}

static PROBE_LOCK: Mutex<()> = Mutex::new(());

fn probe_machine(source: &str) -> Vm {
    let descriptors = vec![
        probe::functions::mark_descriptor(),
        probe::functions::ok_descriptor(),
        probe::functions::err_descriptor(),
    ];
    let catalog = Arc::new(HostFunctionDescriptor::collect_catalog(&descriptors).unwrap());
    let compiled = vm::compile_source_with_flavor_and_options(
        source,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(catalog),
    )
    .expect("probe source should compile");
    let mut registry = HostFunctionRegistry::empty();
    registry.set_capability_profile(vm::CapabilityProfile::deny_all());
    HostModuleDescriptor::install_descriptors(&mut registry, &descriptors).unwrap();
    for descriptor in &descriptors {
        registry.authorize_registered_builtin_import(&descriptor.schema.name);
    }
    let mut machine = Vm::new(compiled.program.with_local_count(compiled.locals));
    registry.bind_vm_cached(&mut machine).unwrap();
    machine.set_fuel(100_000);
    machine
}

fn run_probe(source: &str) -> (Value, Vec<i64>) {
    probe::functions::clear();
    let mut machine = probe_machine(source);
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    let value = machine
        .stack()
        .last()
        .cloned()
        .expect("probe program should leave a value");
    (value, probe::functions::events())
}

#[test]
fn receiver_and_callback_are_evaluated_once_left_to_right() {
    let _guard = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let source = r#"
        use probe;
        fn relay(value: int, ok: bool) -> int {
            let receiver = if ok => { probe::ok(value) } else => { probe::err(value) };
            let mapped = receiver.map(if probe::mark(20) == 20 => { |item| item + probe::mark(30) } else => { |item| item + probe::mark(40) });
            let out = match mapped {
                Result::Ok(item) => item,
                Result::Err(error) => error,
            };
            out
        }
        relay(5, true) + relay(7, false);
    "#;
    let (value, events) = run_probe(source);
    assert_eq!(value, Value::Int(42));
    // Receiver first (5), then the callback expression once (20), then only
    // the selected Ok callback body (30). The Err pass never runs a body.
    assert_eq!(events, vec![5, 20, 30, 7, 20]);
}

#[test]
fn only_the_selected_callback_body_runs_for_err_receivers() {
    let _guard = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let source = r#"
        use probe;
        let receiver = probe::err(8);
        let mapped = receiver.map(|item| item + probe::mark(90));
        let observed = match mapped {
            Result::Ok(item) => item,
            Result::Err(error) => error,
        };
        observed;
    "#;
    let (value, events) = run_probe(source);
    assert_eq!(value, Value::Int(8));
    assert_eq!(events, vec![8]);
}

#[test]
fn eager_unwrap_or_fallback_runs_on_both_branches() {
    let _guard = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let source = r#"
        use probe;
        let ok_value = probe::ok(1);
        let err_value = probe::err(2);
        let ok_out = ok_value.unwrap_or(probe::mark(50));
        let err_out = err_value.unwrap_or(probe::mark(60));
        ok_out + err_out;
    "#;
    let (value, events) = run_probe(source);
    assert_eq!(value, Value::Int(61));
    // The eager fallback runs for the Ok receiver too; only the Err receiver
    // actually selects the fallback value. Both receivers are created first.
    assert_eq!(events, vec![1, 2, 50, 60]);
}

#[test]
fn unwrap_or_else_callback_body_is_lazy() {
    let _guard = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let source = r#"
        use probe;
        let ok_value = probe::ok(3);
        let err_value = probe::err(4);
        let ok_out = ok_value.unwrap_or_else(|error| probe::mark(70) + error);
        let err_out = err_value.unwrap_or_else(|error| probe::mark(80) + error);
        ok_out + err_out;
    "#;
    let (value, events) = run_probe(source);
    assert_eq!(value, Value::Int(3 + 80 + 4));
    // Ok(3) never executes the callback body (no 70); Err(4) does (80).
    assert_eq!(events, vec![3, 4, 80]);
}

#[test]
fn callback_expression_question_mark_exits_before_the_method_branch() {
    let _guard = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let source = r#"
        use probe;
        fn checked(tag: int) -> Result<int, int> {
            Result::Err(tag)
        }
        fn relay() -> Result<int, int> {
            let receiver = probe::ok(9);
            let mapped = receiver.map(if checked(20)? == 20 => { |item| probe::mark(30) + item } else => { |item| probe::mark(40) + item });
            mapped
        }
        let observed = relay();
        let out = match observed {
            Result::Ok(value) => -value,
            Result::Err(error) => error,
        };
        out;
    "#;
    let (value, events) = run_probe(source);
    assert_eq!(value, Value::Int(20));
    // Receiver evaluated first (9), then the callback argument stopped at the
    // `?` exit (20); the branch and both callback bodies never ran (no 30/40).
    assert_eq!(events, vec![9]);
}

#[test]
fn module_function_receiver_and_callbacks_keep_typed_signatures() {
    let lib = r#"
        pub fn provide() -> Result<int, int> {
            Result::Ok(4)
        }
        pub fn transform(value: int) -> int {
            value + 1
        }
        pub fn convert(error: int) -> string {
            "module"
        }
    "#;
    let main = r#"
        use self::lib;
        let mapped = lib::provide().map(transform);
        let payload = match mapped {
            Result::Ok(value) => value,
            Result::Err(error) => -1,
        };
        let failed: Result<int, int> = Result::Err(7);
        let converted = failed.map_err(convert);
        let converted_text = match converted {
            Result::Ok(value) => "none",
            Result::Err(error) => error,
        };
        let observed = if converted_text == "module" => { 1 } else => { 0 };
        payload + observed;
    "#;
    let path = std::path::Path::new("/result-combinator/main.rss");
    let options = CompileSourceFileOptions::new().with_module_override_source("lib.rss", lib);
    let compiled = vm::compile_source_at_path_with_flavor_and_options(
        path,
        main,
        SourceFlavor::RustScript,
        options,
    )
    .expect("module combinator source should compile");
    let mut machine = Vm::new(compiled.program.with_local_count(compiled.locals));
    machine.set_fuel(100_000);
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    assert_eq!(machine.stack().last(), Some(&Value::Int(6)));
}

#[test]
fn repl_entry_types_combinators_with_carried_locals() {
    let source = r#"
        let value: Result<int, int> = Result::Ok(2);
        let mapped = value.map(|item| item + 1);
        let out = match mapped {
            Result::Ok(item) => item,
            Result::Err(error) => -1,
        };
        out;
    "#;
    let compiled =
        vm::compile_source_for_repl_with_locals(source, &[]).expect("REPL source should compile");
    let mut machine = Vm::new(
        compiled
            .compiled
            .program
            .with_local_count(compiled.compiled.locals),
    );
    machine.set_fuel(100_000);
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    assert_eq!(machine.stack().last(), Some(&Value::Int(3)));
}

#[test]
fn analyzer_entry_accepts_typed_combinators() {
    let source = r#"
        let value: Result<int, string> = Result::Ok(1);
        let mapped = value.map(|item| item + 1);
        let failed: Result<int, int> = Result::Err(2);
        let recovered = failed.unwrap_or_else(|error| error + 1);
    "#;
    let model = vm::analyze_source(source).expect("analyzer must accept typed combinators");
    assert!(model.diagnostics().is_empty(), "{:?}", model.diagnostics());
}

#[test]
fn resource_free_temporary_factory_callback_keeps_construction_and_selected_body() {
    let _guard = PROBE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let source = r#"
        use probe;
        fn make_callback(tag: int) -> fn(int) -> int {
            probe::mark(tag);
            |item| item + probe::mark(tag + 1)
        }
        fn relay(value: Result<int, int>, tag: int) -> int {
            let mapped = value.map(make_callback(tag));
            match mapped {
                Result::Ok(item) => item,
                Result::Err(error) => error,
            }
        }
        let ok_out = relay(probe::ok(1), 20);
        let err_out = relay(probe::err(2), 30);
        ok_out + err_out;
    "#;
    let (value, events) = run_probe(source);
    // Ok: 1 -> factory 20 -> body 21 => 22. Err: 2 -> factory 30, no body.
    assert_eq!(value, Value::Int(24));
    assert_eq!(events, vec![1, 20, 21, 2, 30]);
}

#[test]
fn resource_free_local_return_factory_and_forwarding_stay_executable() {
    let _guard = PROBE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let source = r#"
        use probe;
        fn make_callback(tag: int) -> fn(int) -> int {
            let callback = |item| item + tag;
            probe::mark(tag);
            callback
        }
        fn id(callback: fn(int) -> int) -> fn(int) -> int {
            callback
        }
        fn relay(value: Result<int, int>, tag: int) -> int {
            let mapped = value.map(id(make_callback(tag)));
            match mapped {
                Result::Ok(item) => item,
                Result::Err(error) => error,
            }
        }
        let ok_out = relay(probe::ok(1), 20);
        let err_out = relay(probe::err(2), 30);
        ok_out + err_out;
    "#;
    let (value, events) = run_probe(source);
    // Ok: 1 -> factory 20 -> selected body 1 + 20 => 21. Err: 2 -> factory 30,
    // the unselected body never runs and the error payload passes through.
    assert_eq!(value, Value::Int(23));
    assert_eq!(events, vec![1, 20, 2, 30]);
}

#[test]
fn resource_free_statement_if_and_alias_reassignment_stay_executable() {
    let _guard = PROBE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let source = r#"
        use probe;
        fn make_if(tag: int, flag: bool) -> fn(int) -> int {
            let mut callback = |value| value + tag;
            if flag {
                callback = |value| value + probe::mark(tag + 1);
            } else {
                callback = |value| value + tag;
            }
            callback
        }
        fn make_alias(tag: int) -> fn(int) -> int {
            let mut callback = |value| value + tag;
            let alias = callback;
            callback = |value| value + probe::mark(tag + 2);
            alias
        }
        fn relay_if(value: Result<int, int>, tag: int, flag: bool) -> int {
            let mapped = value.map(make_if(tag, flag));
            match mapped {
                Result::Ok(item) => item,
                Result::Err(error) => error,
            }
        }
        fn relay_alias(value: Result<int, int>, tag: int) -> int {
            let mapped = value.map(make_alias(tag));
            match mapped {
                Result::Ok(item) => item,
                Result::Err(error) => error,
            }
        }
        let selected = relay_if(probe::ok(1), 10, true);
        let fallback = relay_if(probe::ok(2), 20, false);
        let aliased = relay_alias(probe::ok(3), 30);
        selected + fallback + aliased;
    "#;
    let (value, events) = run_probe(source);
    // Then branch: 1 + mark(11) = 12. Else branch: 2 + 20 = 22. The alias
    // keeps the callback bound before the source slot is reassigned:
    // 3 + 30 = 33. 12 + 22 + 33 = 67.
    assert_eq!(value, Value::Int(12 + 22 + 33));
    assert_eq!(events, vec![1, 11, 2, 3]);
}

#[test]
fn resource_free_frozen_alias_before_shared_writer_stays_executable() {
    let _guard = PROBE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let source = r#"
        use probe;
        fn make_free(tag: int) -> fn(int) -> int {
            |item| item
        }
        fn make_writing(tag: int) -> fn(int) -> int {
            |item| item + probe::mark(tag)
        }
        fn make(tag: int) -> fn(int) -> int {
            let mut factory = make_free;
            let alias = factory;
            let writer = || if true => {
                factory = make_writing;
                0
            } else => {
                factory = make_free;
                0
            };
            writer();
            alias(tag)
        }
        fn relay(value: Result<int, int>, tag: int) -> int {
            let mapped = value.map(make(tag));
            match mapped {
                Result::Ok(item) => item,
                Result::Err(error) => error,
            }
        }
        let ok_out = relay(probe::ok(1), 20);
        let err_out = relay(probe::err(2), 30);
        ok_out + err_out;
    "#;
    let (value, events) = run_probe(source);
    // The alias keeps the target frozen before the writer barrier. make_writing
    // must not run, and map still passes both payloads through.
    assert_eq!(value, Value::Int(3));
    assert_eq!(events, vec![1, 2]);
}

#[test]
fn resource_free_scalar_loop_in_temporary_factory_stays_executable() {
    let _guard = PROBE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let source = r#"
        use probe;
        fn make_while(n: int, tag: int) -> fn(int) -> int {
            let mut total = 0;
            let mut index = 0;
            while index < n {
                total = total + index;
                index = index + 1;
            }
            let callback = |value| value + total + tag;
            callback
        }
        fn make_for(n: int, tag: int) -> fn(int) -> int {
            let mut total = 0;
            for index in 0..n {
                total = total + index;
            }
            let callback = |value| value + total + tag;
            callback
        }
        fn relay_while(value: Result<int, int>, n: int, tag: int) -> int {
            let mapped = value.map(make_while(n, tag));
            match mapped {
                Result::Ok(item) => item,
                Result::Err(error) => error,
            }
        }
        fn relay_for(value: Result<int, int>, n: int, tag: int) -> int {
            let mapped = value.map(make_for(n, tag));
            match mapped {
                Result::Ok(item) => item,
                Result::Err(error) => error,
            }
        }
        let while_out = relay_while(probe::ok(4), 3, 7);
        let for_out = relay_for(probe::ok(5), 3, 9);
        while_out + for_out;
    "#;
    let (value, events) = run_probe(source);
    // while total 0+1+2=3: 4+3+7=14; for total 3: 5+3+9=17.
    assert_eq!(value, Value::Int(31));
    assert_eq!(events, vec![4, 5]);
}

#[test]
fn typed_unwrap_payload_in_function_tail_and_annotated_let() {
    let _guard = PROBE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let source = r#"
        use probe;
        fn pick_tail(value: Result<int, int>) -> int {
            value.unwrap_or(probe::mark(10))
        }
        fn pick_let(value: Result<int, int>) -> int {
            let out: int = value.unwrap_or(probe::mark(11));
            out
        }
        fn pick_else_tail(value: Result<int, int>) -> int {
            value.unwrap_or_else(|error| probe::mark(12) + error)
        }
        fn pick_else_let(value: Result<int, int>) -> int {
            let out: int = value.unwrap_or_else(|error| probe::mark(13) + error);
            out
        }
        let ok_out = pick_tail(probe::ok(1));
        let err_let = pick_let(probe::err(2));
        let ok_else = pick_else_tail(probe::ok(3));
        let err_else_let = pick_else_let(probe::err(4));
        ok_out + err_let + ok_else + err_else_let;
    "#;
    let (value, events) = run_probe(source);
    // Ok(1) selects 1 after the eager fallback mark 10; Err(2) selects the
    // eager fallback 11; Ok(3) leaves the lazy callback body unrun; Err(4)
    // runs the lazy body (13 + 4 = 17). 1 + 11 + 3 + 17 = 32.
    assert_eq!(value, Value::Int(32));
    assert_eq!(events, vec![1, 10, 2, 11, 3, 4, 13]);
}

#[test]
fn typed_unwrap_payload_supports_non_int_and_nested_result() {
    let source = r#"
        fn pick_text(value: Result<string, string>) -> string {
            value.unwrap_or("fallback")
        }
        fn pick_text_else(value: Result<string, string>) -> string {
            value.unwrap_or_else(|error| error)
        }
        fn pick_nested(value: Result<Result<int, int>, int>) -> Result<int, int> {
            value.unwrap_or(Result::Ok(7))
        }
        fn pick_nested_else(value: Result<Result<int, int>, int>) -> Result<int, int> {
            value.unwrap_or_else(|error| Result::Ok(error + 1))
        }
        let ok_text: Result<string, string> = Result::Ok("ok");
        let err_text: Result<string, string> = Result::Err("bad");
        let text_out = pick_text(ok_text);
        let text_else_out = pick_text_else(err_text);
        let inner: Result<int, int> = Result::Ok(5);
        let outer_ok: Result<Result<int, int>, int> = Result::Ok(inner);
        let outer_err: Result<Result<int, int>, int> = Result::Err(6);
        let nested_ok = pick_nested(outer_ok);
        let nested_fallback = pick_nested(outer_err);
        let nested_else_ok = pick_nested_else(outer_ok);
        let nested_else_err = pick_nested_else(outer_err);
        let nested_ok_payload = match nested_ok {
            Result::Ok(item) => item,
            Result::Err(error) => -error,
        };
        let nested_fallback_payload = match nested_fallback {
            Result::Ok(item) => item,
            Result::Err(error) => -error,
        };
        let nested_else_ok_payload = match nested_else_ok {
            Result::Ok(item) => item,
            Result::Err(error) => -error,
        };
        let nested_else_err_payload = match nested_else_err {
            Result::Ok(item) => item,
            Result::Err(error) => -error,
        };
        let text_flag = if text_out == "ok" && text_else_out == "bad" => { 1 } else => { 0 };
        text_flag + nested_ok_payload + nested_fallback_payload + nested_else_ok_payload
            + nested_else_err_payload;
    "#;
    assert_eq!(eval(source), Value::Int(1 + 5 + 7 + 5 + 7));
}

#[test]
fn result_returning_combinators_still_reject_non_result_annotations() {
    for (label, source) in [
        (
            "map",
            r#"
        fn bad(value: Result<int, int>) -> int {
            value.map(|item| item + 1)
        }
        "#,
        ),
        (
            "and_then",
            r#"
        fn bad(value: Result<int, int>) -> int {
            value.and_then(|item| Result::Ok(item + 1))
        }
        "#,
        ),
    ] {
        let diagnostic = reject(source);
        assert!(
            diagnostic.contains("produces a Result value, not int"),
            "{label}: {diagnostic}"
        );
    }
}

#[test]
fn result_map_known_named_ok_schema_materializes_omitted_option_field() {
    let source = r#"
        struct Entry { name: string, note: Option<int> }
        fn observe_note(note: Option<Option<int>>) -> int {
            match note {
                Option::Some(inner_note) => match inner_note {
                    Option::Some(value) => value,
                    Option::None => 1,
                },
                Option::None => -2,
            }
        }
        fn observe_entry(value: Entry) -> int {
            observe_note(value?.note)
        }
        let flag: Result<int, string> = Result::Ok(1);
        let item: Result<Entry, string> = flag.map(|n| { name: "x" });
        match item {
            Result::Ok(entry) => observe_entry(entry),
            Result::Err(error) => -1,
        };
    "#;
    assert_eq!(eval(source), Value::Int(1));
}

#[test]
fn result_map_err_known_named_error_schema_materializes_omitted_option_field() {
    let source = r#"
        struct Failure { code: int, detail: Option<int> }
        fn observe_note(note: Option<Option<int>>) -> int {
            match note {
                Option::Some(inner_note) => match inner_note {
                    Option::Some(value) => value,
                    Option::None => 1,
                },
                Option::None => -2,
            }
        }
        fn observe_failure(error: Failure) -> int {
            observe_note(error?.detail)
        }
        let flag: Result<int, int> = Result::Err(7);
        let item: Result<int, Failure> = flag.map_err(|error| { code: error });
        match item {
            Result::Ok(value) => -1,
            Result::Err(error) => observe_failure(error),
        };
    "#;
    assert_eq!(eval(source), Value::Int(1));
}

#[test]
fn result_unwrap_or_and_unwrap_or_else_known_named_payload_materialize_none() {
    let source = r#"
        struct Entry { name: string, note: Option<int> }
        fn observe_note(note: Option<Option<int>>) -> int {
            match note {
                Option::Some(inner_note) => match inner_note {
                    Option::Some(value) => value,
                    Option::None => 1,
                },
                Option::None => -2,
            }
        }
        fn observe_entry(value: Entry) -> int {
            observe_note(value?.note)
        }
        let absent: Result<Entry, int> = Result::Err(3);
        let eager: Entry = absent.unwrap_or({ name: "eager" });
        let lazy: Entry = absent.unwrap_or_else(|error| { name: "lazy" });
        observe_entry(eager) + observe_entry(lazy);
    "#;
    assert_eq!(eval(source), Value::Int(2));
}

#[test]
fn result_and_then_and_or_else_known_named_payloads_materialize_none() {
    let source = r#"
        struct Entry { name: string, note: Option<int> }
        fn observe_note(note: Option<Option<int>>) -> int {
            match note {
                Option::Some(inner_note) => match inner_note {
                    Option::Some(value) => value,
                    Option::None => 1,
                },
                Option::None => -2,
            }
        }
        fn observe_entry(value: Entry) -> int {
            observe_note(value?.note)
        }
        let ok_value: Result<int, string> = Result::Ok(2);
        let chained: Result<Entry, string> = ok_value.and_then(|n| Result::Ok({ name: "x" }));
        let err_value: Result<Entry, int> = Result::Err(5);
        let recovered: Result<Entry, int> = err_value.or_else(|error| Result::Ok({ name: "y" }));
        match chained {
            Result::Ok(entry) => observe_entry(entry),
            Result::Err(error) => -1,
        } + match recovered {
            Result::Ok(entry) => observe_entry(entry),
            Result::Err(error) => -1,
        };
    "#;
    assert_eq!(eval(source), Value::Int(2));
}

#[test]
fn typed_unwrap_payload_still_requires_the_payload_type() {
    for (label, source) in [
        (
            "unwrap_or",
            r#"
        fn bad(value: Result<int, int>) -> string {
            value.unwrap_or(0)
        }
        "#,
        ),
        (
            "unwrap_or_else",
            r#"
        fn bad(value: Result<int, int>) -> string {
            value.unwrap_or_else(|error| error)
        }
        "#,
        ),
    ] {
        let diagnostic = reject(source);
        assert!(
            diagnostic.contains(label) && diagnostic.contains("expects string"),
            "{label}: {diagnostic}"
        );
    }
}

#[test]
fn result_unwrap_or_rejects_nonconstruction_sparse_fallback() {
    let diagnostic = reject(
        r#"
        struct Entry { name: string, note: Option<int> }
        fn make(flag: Result<Entry, int>) -> Entry {
            let raw = { name: "x" };
            flag.unwrap_or(raw)
        }
        make(Result::Err(3)).name;
        "#,
    );
    assert!(
        diagnostic.contains("unwrap_or")
            && diagnostic.contains("field 'note'")
            && diagnostic.contains("must be materialized"),
        "the Result eager fallback must be rejected before the named label is installed: {diagnostic}"
    );
}

#[test]
fn result_unwrap_or_keeps_a_previously_materialized_named_fallback() {
    let source = r#"
        struct Entry { name: string, note: Option<int> }
        let fallback: Entry = { name: "x" };
        let absent: Result<Entry, int> = Result::Err(3);
        let item: Entry = absent.unwrap_or(fallback);
        match item.note { Option::Some(value) => value, Option::None => 0 };
    "#;
    assert_eq!(eval(source), Value::Int(0));
}
