#![cfg(all(feature = "runtime", feature = "cranelift-jit"))]

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use vm::{CallOutcome, CallReturn, HostFunction, HostTypeSchema, Value, Vm, VmResult, VmStatus};

struct Input(Value, Arc<AtomicUsize>);
impl HostFunction for Input {
    fn call(&mut self, _: &mut Vm, _: &[Value]) -> VmResult<CallOutcome> {
        self.1.fetch_add(1, Ordering::SeqCst);
        Ok(CallOutcome::Return(CallReturn::one(self.0.clone())))
    }
}

fn dynamic_payload(ty: &str, schema: HostTypeSchema, payload: Value, ok: bool, mode: &str) {
    let source = format!(
        r#"
        pub fn increment(n: int) -> int {{ n + 1 }}
        pub fn relay(flag: bool, payload: {ty}) -> Result<{ty}, {ty}> {{
            let carrier: Result<{ty}, {ty}> = if flag => {{ Result::Ok(payload) }} else => {{ Result::Err(payload) }};
            let mut i = 0;
            let mut selected: {ty} = payload;
            while i < 64 {{
                selected = match carrier {{ Result::Ok(v) => v, Result::Err(e) => e, }};
                i = i + 1;
            }}
            if flag => {{ Result::Ok(selected) }} else => {{ Result::Err(selected) }}
        }}
        relay(input::flag(), input::value());
    "#
    );
    let mut builder = vm::HostApiBuilder::new();
    builder.function(vm::HostFunctionSchema::with_return(
        "input::flag",
        vec![],
        HostTypeSchema::Bool,
    ));
    builder.function(vm::HostFunctionSchema::with_return(
        "input::value",
        vec![],
        schema,
    ));
    let compiled = vm::compile_source_with_flavor_and_options(
        &source,
        vm::SourceFlavor::RustScript,
        vm::CompileSourceFileOptions::new()
            .with_host_api_catalog(Arc::new(builder.build().unwrap())),
    )
    .unwrap();
    let mut machine = Vm::new_with_jit_config(
        compiled.program.with_local_count(compiled.locals),
        vm::JitConfig {
            enabled: mode == "trace",
            hot_loop_threshold: 1,
            max_trace_len: 512,
        },
    );
    let payload = if ty == "fn(int) -> int" {
        machine.resolve_exported_callable("increment").unwrap()
    } else {
        payload
    };
    let flag_calls = Arc::new(AtomicUsize::new(0));
    let value_calls = Arc::new(AtomicUsize::new(0));
    machine.bind_function(
        "input::flag",
        Box::new(Input(Value::Bool(ok), flag_calls.clone())),
    );
    machine.bind_function(
        "input::value",
        Box::new(Input(payload.clone(), value_calls.clone())),
    );
    machine.set_fuel(100_000);
    if mode == "aot" {
        machine
            .compile_aot()
            .unwrap_or_else(|e| panic!("{ty} ok={ok} AOT admission: {e:?}"));
        let dump = machine.dump_aot_info();
        assert!(
            dump.contains("lowering=ssa") && !dump.contains("interpreter-boundary"),
            "{dump}"
        );
        eprintln!("R4 AOT {ty} ok={ok}: {dump}");
    }
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    assert_eq!(flag_calls.load(Ordering::SeqCst), 1);
    assert_eq!(value_calls.load(Ordering::SeqCst), 1);
    let result = machine.stack().last().unwrap();
    let (tag, selected) = vm::result::payload(result).unwrap();
    assert_eq!(tag, ok);
    if let (Value::Float(actual), Value::Float(expected)) = (selected, &payload) {
        if expected.is_nan() {
            assert!(actual.is_nan());
        } else {
            assert_eq!(actual.to_bits(), expected.to_bits());
            assert_eq!(actual.is_sign_negative(), expected.is_sign_negative());
        }
    } else {
        assert_eq!(selected, &payload);
    }
    if mode == "aot" {
        assert!(machine.aot_exec_count() > 0);
    }
    if mode == "trace" {
        let snapshot = machine.jit_snapshot();
        let relevant: Vec<_> = snapshot
            .traces
            .iter()
            .filter(|t| {
                t.executions > 0
                    && t.op_names().iter().any(|op| op == "map_get")
                    && t.ssa_text().contains("payload")
            })
            .collect();
        eprintln!(
            "R4 trace {ty} ok={ok} native={} relevant={}\n{}",
            machine.jit_native_exec_count(),
            relevant.len(),
            machine
                .dump_jit_info()
                .lines()
                .filter(|line| !line.trim_start().starts_with("code:"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert!(
            !relevant.is_empty(),
            "required native Result payload region absent: {:?}",
            snapshot.attempts
        );
    }
    eprintln!(
        "R4 parity mode={mode} ty={ty} ok={ok} payload={selected:?} host_calls=1/1 aot={} trace={}",
        machine.aot_exec_count(),
        machine.jit_native_exec_count()
    );
    if ty == "fn(int) -> int" {
        let alias = selected.clone();
        assert_eq!(
            machine.invoke_callable(alias, &[Value::Int(41)]).unwrap(),
            Value::Int(42)
        );
    }
}

#[test]
fn dynamic_int_float_bool_optional_and_nested_payloads_all_backends() {
    let vectors = [
        (
            "fn(int) -> int",
            HostTypeSchema::Callable {
                params: vec![HostTypeSchema::Int],
                result: Box::new(HostTypeSchema::Int),
            },
            Value::Null,
        ),
        ("int", HostTypeSchema::Int, Value::Int(-37)),
        ("float", HostTypeSchema::Float, Value::Float(-0.0)),
        ("float", HostTypeSchema::Float, Value::Float(f64::NAN)),
        ("bool", HostTypeSchema::Bool, Value::Bool(false)),
        (
            "int?",
            HostTypeSchema::Optional(Box::new(HostTypeSchema::Int)),
            Value::Null,
        ),
        (
            "int?",
            HostTypeSchema::Optional(Box::new(HostTypeSchema::Int)),
            Value::Int(29),
        ),
        (
            "Result<int, bool>",
            HostTypeSchema::Result {
                ok: Box::new(HostTypeSchema::Int),
                err: Box::new(HostTypeSchema::Bool),
            },
            vm::result::carrier(false, Value::Bool(false)),
        ),
        (
            "Result<int, bool>",
            HostTypeSchema::Result {
                ok: Box::new(HostTypeSchema::Int),
                err: Box::new(HostTypeSchema::Bool),
            },
            vm::result::carrier(true, Value::Int(0)),
        ),
    ];
    for (ty, schema, payload) in vectors {
        for mode in ["interpreter", "trace", "aot"] {
            for ok in [true, false] {
                dynamic_payload(ty, schema.clone(), payload.clone(), ok, mode);
            }
        }
    }
}

#[test]
fn dynamic_callback_try_native_and_malformed_admission_before_body() {
    for mode in ["interpreter", "trace", "aot"] {
        let mut builder = vm::HostApiBuilder::new();
        builder.function(vm::HostFunctionSchema::with_return(
            "input::tick",
            vec![],
            HostTypeSchema::Int,
        ));
        let compiled = vm::compile_source_with_flavor_and_options(
            r#"
            pub fn checked(value: Result<int, string>) -> Result<int, string> {
                let before = input::tick();
                let mut i = 0;
                let mut sum = 0;
                while i < 64 { let n = value?; sum = sum + n; i = i + 1; }
                let after = input::tick();
                Result::Ok(sum)
            }
            0;
        "#,
            vm::SourceFlavor::RustScript,
            vm::CompileSourceFileOptions::new()
                .with_host_api_catalog(Arc::new(builder.build().unwrap())),
        )
        .unwrap();
        let mut machine = Vm::new_with_jit_config(
            compiled.program.with_local_count(compiled.locals),
            vm::JitConfig {
                enabled: mode == "trace",
                hot_loop_threshold: 1,
                max_trace_len: 512,
            },
        );
        let calls = Arc::new(AtomicUsize::new(0));
        machine.bind_function("input::tick", Box::new(Input(Value::Int(1), calls.clone())));
        machine.set_fuel(100_000);
        if mode == "aot" {
            machine.compile_aot().expect("standalone callback AOT");
        }
        assert_eq!(machine.run().unwrap(), VmStatus::Halted);
        let callable = machine.resolve_exported_callable("checked").unwrap();
        for ok in [true, false] {
            let before = calls.load(Ordering::SeqCst);
            let native_before = (machine.jit_native_exec_count(), machine.aot_exec_count());
            let value = if ok {
                vm::result::carrier(true, Value::Int(3))
            } else {
                vm::result::carrier(false, Value::string("dynamic-error"))
            };
            let returned = machine.invoke_callable(callable.clone(), &[value]).unwrap();
            let native_delta = (
                machine.jit_native_exec_count() - native_before.0,
                machine.aot_exec_count() - native_before.1,
            );
            if mode == "trace" {
                assert!(
                    native_delta.0 > 0,
                    "callback {ok} must execute native Result region"
                );
            }
            if mode == "aot" {
                assert!(native_delta.1 > 0, "callback {ok} must execute AOT");
            }
            eprintln!("R4 callback branch mode={mode} ok={ok} native_delta={native_delta:?}");
            let (tag, payload) = vm::result::payload(&returned).unwrap();
            assert_eq!(tag, ok);
            assert_eq!(
                payload,
                &if ok {
                    Value::Int(192)
                } else {
                    Value::string("dynamic-error")
                }
            );
            assert_eq!(
                calls.load(Ordering::SeqCst) - before,
                if ok { 2 } else { 1 }
            );
        }
        for malformed in [
            Value::Null,
            Value::map(vec![
                (Value::string("tag"), Value::Int(1)),
                (Value::string("payload"), Value::Int(1)),
            ]),
            vm::result::carrier(true, Value::Bool(false)),
            Value::map(vec![(Value::string("tag"), Value::Bool(true))]),
            Value::map(vec![
                (Value::string("tag"), Value::Bool(true)),
                (Value::string("payload"), Value::Int(1)),
                (Value::string("extra"), Value::Int(0)),
            ]),
        ] {
            let before = calls.load(Ordering::SeqCst);
            assert!(
                machine
                    .invoke_callable(callable.clone(), &[malformed])
                    .is_err()
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                before,
                "admission must precede guest body"
            );
        }
        if mode == "aot" {
            assert!(machine.aot_exec_count() > 1);
        }
        if mode == "trace" {
            let snapshot = machine.jit_snapshot();
            assert!(
                snapshot.traces.iter().any(|t| t.executions > 0
                    && t.ssa_text().contains("payload")
                    && t.op_names().iter().any(|op| op == "map_get")),
                "{:?}",
                snapshot.attempts
            );
        }
        eprintln!(
            "R4 standalone callback mode={mode} aot={} trace={} ticks={} malformed=5 prebody_rejected",
            machine.aot_exec_count(),
            machine.jit_native_exec_count(),
            calls.load(Ordering::SeqCst)
        );
    }
}

#[test]
fn dynamic_manual_record_bool_branch_aot_checks_tagged_representation() {
    let source = "struct Pair { tag: bool, payload: int } fn choose(flag: bool) -> int { let pair: Pair = { tag: flag, payload: 7 }; let n = if pair.tag => { pair.payload } else => { 9 }; n } choose(true);";
    let compiled = vm::compile_source(source).unwrap();
    let mut machine = Vm::new_with_jit_config(
        compiled.program.with_local_count(compiled.locals),
        vm::JitConfig {
            enabled: false,
            hot_loop_threshold: 1,
            max_trace_len: 512,
        },
    );
    machine.set_fuel(100_000);
    machine
        .compile_aot()
        .expect("ordinary typed record Bool branch must unbox tagged Get");
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    assert_eq!(machine.stack().last(), Some(&Value::Int(7)));
    assert!(machine.aot_exec_count() > 0);
}

#[test]
fn result_artifact_schema_binding_reset_vm_replacement_and_cross_vm() {
    type R = vm::GuestResult<i64, String>;
    let source = "pub fn echo(value: Result<int, string>) -> Result<int, string> { value } 0;";
    let compiled = vm::compile_source(source).unwrap();
    let bytes = vm::encode_program(&compiled.program.with_local_count(compiled.locals)).unwrap();
    let new_vm = || {
        let mut machine = Vm::new_with_jit_config(
            vm::decode_program(&bytes).unwrap(),
            vm::JitConfig {
                enabled: false,
                hot_loop_threshold: 1,
                max_trace_len: 512,
            },
        );
        machine.set_fuel(100_000);
        machine
    };
    let mut store = vm::Store::from_vm(new_vm());
    store.vm_mut().compile_aot().unwrap();
    let artifact = store.vm_mut().encode_aot_artifact().unwrap();
    assert_eq!(store.run().unwrap(), VmStatus::Halted);
    let callback = store.script_callback_by_name::<(R,), R>("echo").unwrap();
    assert_eq!(callback.call(&mut store, (R::Ok(7),)).unwrap(), R::Ok(7));
    let mut other = vm::Store::from_vm(new_vm());
    other.run().unwrap();
    assert!(callback.call(&mut other, (R::Ok(7),)).is_err());
    let mut loaded = new_vm();
    loaded.load_aot_artifact(&artifact).unwrap();
    let mut loaded = vm::Store::from_vm(loaded);
    loaded.run().unwrap();
    let echo = loaded.script_callback_by_name::<(R,), R>("echo").unwrap();
    let before = loaded.vm().aot_exec_count();
    assert_eq!(
        echo.call(&mut loaded, (R::Err("loaded".into()),)).unwrap(),
        R::Err("loaded".into())
    );
    assert!(loaded.vm().aot_exec_count() > before);
    let mismatch = vm::compile_source(&source.replace("string", "bool")).unwrap();
    let original = vm::decode_program(&bytes).unwrap();
    assert_eq!(mismatch.program.code, original.code);
    assert_ne!(
        mismatch.program.callable_prototypes[0].schema,
        original.callable_prototypes[0].schema
    );
    let mut mismatch = Vm::new(mismatch.program.with_local_count(mismatch.locals));
    let error = mismatch
        .load_aot_artifact(&artifact)
        .expect_err("different Result error schema must reject");
    assert!(matches!(
        error,
        vm::AotArtifactError::IncompatibleProgramHash { .. }
    ));
    store.reset_for_reuse().unwrap();
    assert!(!store.vm().scope_reset_pending());
    assert!(callback.prepare((R::Ok(7),)).is_err());
    store.vm_mut().set_fuel(100_000);
    store.run().unwrap();
    let old = store.script_callback_by_name::<(R,), R>("echo").unwrap();
    let replacement = vm::compile_source("pub fn echo(value: Result<int, string>) -> Result<int, string> { Result::Err(\"replacement\") } 0;").unwrap();
    *store.vm_mut() = Vm::new(replacement.program.with_local_count(replacement.locals));
    assert!(
        old.call(&mut store, (R::Ok(1),)).is_err(),
        "replacement must reject the old callable at Program admission"
    );
    store.reset_for_reuse().unwrap();
    assert!(old.prepare((R::Ok(1),)).is_err());
    store.vm_mut().set_fuel(100_000);
    store.run().unwrap();
    let fresh = store.script_callback_by_name::<(R,), R>("echo").unwrap();
    assert_eq!(
        fresh.call(&mut store, (R::Ok(1),)).unwrap(),
        R::Err("replacement".into())
    );
    eprintln!(
        "R4 artifact schema mismatch rejected; loaded Result Err executes natively; reset/cross-VM/replacement revoke stale callbacks"
    );
}

#[test]
fn dynamic_captured_closure_generic_alias_try_all_backends() {
    for mode in ["interpreter", "trace", "aot"] {
        for ok in [true, false] {
            let mut builder = vm::HostApiBuilder::new();
            builder.function(vm::HostFunctionSchema::with_return(
                "input::flag",
                vec![],
                HostTypeSchema::Bool,
            ));
            builder.function(vm::HostFunctionSchema::with_return(
                "input::value",
                vec![],
                HostTypeSchema::Int,
            ));
            let source = r#"
                fn apply<T, E>(callback: fn() -> Result<T, E>) -> Result<T, E> { callback() }
                pub fn runner(flag: bool, seed: int) -> Result<int, int> {
                    let callback: fn() -> Result<int, int> = || if true => {
                        let mut i = 0; let mut sum = 0;
                        while i < 64 {
                            let step: Result<int, int> = if flag || i != 32 => { Result::Ok(seed) } else => { Result::Err(sum) };
                            let n = step?; sum = n; i = i + 1;
                        }
                        Result::Ok(sum)
                    } else => { Result::Err(-1) };
                    let alias = callback;
                    apply::<int, int>(alias)
                }
                runner(input::flag(), input::value());
            "#;
            let compiled = vm::compile_source_with_flavor_and_options(
                source,
                vm::SourceFlavor::RustScript,
                vm::CompileSourceFileOptions::new()
                    .with_host_api_catalog(Arc::new(builder.build().unwrap())),
            )
            .unwrap();
            if mode == "aot" {
                eprintln!(
                    "R4 captured metadata functions={:?} prototypes={:?} regions={:?}\n{}",
                    compiled.program.script_functions,
                    compiled.program.callable_prototypes,
                    compiled.program.function_regions,
                    vm::disassemble_program(&compiled.program)
                );
            }
            let mut machine = Vm::new_with_jit_config(
                compiled.program.with_local_count(compiled.locals),
                vm::JitConfig {
                    enabled: mode == "trace",
                    hot_loop_threshold: 1,
                    max_trace_len: 512,
                },
            );
            let flags = Arc::new(AtomicUsize::new(0));
            let values = Arc::new(AtomicUsize::new(0));
            machine.bind_function(
                "input::flag",
                Box::new(Input(Value::Bool(ok), flags.clone())),
            );
            machine.bind_function(
                "input::value",
                Box::new(Input(Value::Int(7), values.clone())),
            );
            machine.set_fuel(100_000);
            if mode == "aot" {
                machine
                    .compile_aot()
                    .expect("captured Result closure native AOT");
            }
            assert_eq!(machine.run().unwrap(), VmStatus::Halted);
            let (tag, payload) = vm::result::payload(machine.stack().last().unwrap()).unwrap();
            assert_eq!(tag, ok);
            assert_eq!(payload, &Value::Int(7));
            assert_eq!(flags.load(Ordering::SeqCst), 1);
            assert_eq!(values.load(Ordering::SeqCst), 1);
            if mode == "aot" {
                assert!(machine.aot_exec_count() > 0);
            }
            if mode == "trace" {
                assert!(
                    machine
                        .jit_snapshot()
                        .traces
                        .iter()
                        .any(|t| t.executions > 0
                            && t.ssa_text().contains("payload")
                            && t.op_names().iter().any(|op| op == "map_get")),
                    "{}",
                    machine.dump_jit_info()
                );
            }
            eprintln!(
                "R4 captured generic alias mode={mode} ok={ok} payload={payload:?} host_calls=1/1 trace={} aot={}",
                machine.jit_native_exec_count(),
                machine.aot_exec_count()
            );
        }
    }
}

#[test]
fn dynamic_string_payload_both_branches_all_backends() {
    for mode in ["interpreter", "trace", "aot"] {
        for ok in [true, false] {
            dynamic_payload(
                "string",
                HostTypeSchema::String,
                Value::string("runtime-payload"),
                ok,
                mode,
            );
        }
    }
}
