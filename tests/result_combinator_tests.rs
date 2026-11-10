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
