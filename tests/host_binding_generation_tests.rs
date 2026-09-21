#[allow(dead_code)]
#[path = "../build.rs"]
mod build_script;

use build_script::{
    HostBindingKind, HostExecutionKind, callable_param_expr, classify_host_binding,
    infer_host_execution,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use syn::parse_quote;
use vm::{
    BuiltinFunction, BytecodeBuilder, CallOutcome, CallReturn, CapabilityProfile, HostApiBuilder,
    HostFunction, HostFunctionRegistry, HostFunctionSchema, HostImport, HostImportSchema,
    HostParamSchema, HostTypeSchema, JitConfig, JitTraceTerminal, Program, Value, Vm, VmStatus,
    compile_source,
};

fn schema_for(function: &HostFunctionSchema) -> (HostImport, HostImportSchema) {
    let mut builder = HostApiBuilder::new();
    builder.function(function.clone());
    let catalog = builder.build().expect("test catalog");
    let schema = HostImportSchema::from_function(&catalog, function);
    let import = HostImport {
        name: function.name.clone(),
        arity: function.params.len() as u8,
        return_type: match function.return_type {
            HostTypeSchema::Null => vm::ValueType::Null,
            HostTypeSchema::Int => vm::ValueType::Int,
            HostTypeSchema::Float => vm::ValueType::Float,
            HostTypeSchema::Number => vm::ValueType::Float,
            HostTypeSchema::Bool => vm::ValueType::Bool,
            HostTypeSchema::String => vm::ValueType::String,
            HostTypeSchema::Bytes => vm::ValueType::Bytes,
            HostTypeSchema::Array(_) => vm::ValueType::Array,
            HostTypeSchema::Map(_) | HostTypeSchema::Named { .. } => vm::ValueType::Map,
            HostTypeSchema::Callable { .. } => vm::ValueType::Callable,
            HostTypeSchema::Resource(_) | HostTypeSchema::Optional(_) | HostTypeSchema::Unknown => {
                vm::ValueType::Unknown
            }
        },
    };
    (import, schema)
}

fn program_with_imports(
    imports: Vec<HostImport>,
    schemas: Vec<HostImportSchema>,
    code: Vec<u8>,
) -> Arc<Program> {
    Arc::new(
        Program::with_imports_and_debug(Vec::new(), code, imports, None)
            .with_host_import_schemas(schemas)
            .expect("aligned host schemas"),
    )
}

fn return_int(_vm: &mut Vm, _args: &[Value]) -> vm::VmResult<CallOutcome> {
    Ok(CallOutcome::Return(CallReturn::one(Value::Int(7))))
}

struct IsolatedCounterHost {
    calls: usize,
}

impl HostFunction for IsolatedCounterHost {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> vm::VmResult<CallOutcome> {
        self.calls += 1;
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(
            self.calls as i64,
        ))))
    }
}

#[test]
fn one_bound_program_constructs_ten_thousand_vms() {
    let function = HostFunctionSchema::with_return("bound::value", Vec::new(), HostTypeSchema::Int);
    let (import, schema) = schema_for(&function);
    let mut bytecode = BytecodeBuilder::new();
    bytecode.ret();
    let program = program_with_imports(vec![import], vec![schema.clone()], bytecode.finish());

    let mut registry = HostFunctionRegistry::empty();
    registry
        .register_catalog_static(schema, return_int)
        .expect("static binding");
    let bound = registry
        .bind_program_once(Arc::clone(&program))
        .expect("prepare bound program");

    for _ in 0..10_000 {
        let vm = Vm::new_bound(Arc::clone(&bound)).expect("construct bound vm");
        assert_eq!(vm.bound_function_count(), 1);
    }
}

#[test]
fn bound_program_keeps_mutable_host_state_per_vm() {
    let function =
        HostFunctionSchema::with_return("bound::counter", Vec::new(), HostTypeSchema::Int);
    let (import, schema) = schema_for(&function);
    let mut bytecode = BytecodeBuilder::new();
    bytecode.call(0, 0);
    bytecode.ret();
    let program = program_with_imports(vec![import], vec![schema.clone()], bytecode.finish());

    let factory_count = Arc::new(AtomicUsize::new(0));
    let mut registry = HostFunctionRegistry::empty();
    registry
        .register_catalog(schema, {
            let factory_count = Arc::clone(&factory_count);
            move || {
                factory_count.fetch_add(1, Ordering::SeqCst);
                Box::new(IsolatedCounterHost { calls: 0 })
            }
        })
        .expect("counter binding");
    let bound = registry
        .bind_program_once(Arc::clone(&program))
        .expect("prepare counter binding");
    assert_eq!(factory_count.load(Ordering::SeqCst), 0);

    let mut first = Vm::new_bound(Arc::clone(&bound)).expect("first bound vm");
    let mut second = Vm::new_bound(bound).expect("second bound vm");
    assert_eq!(factory_count.load(Ordering::SeqCst), 2);
    assert_eq!(first.run().expect("first run"), VmStatus::Halted);
    assert_eq!(second.run().expect("second run"), VmStatus::Halted);
    assert_eq!(first.stack(), &[Value::Int(1)]);
    assert_eq!(second.stack(), &[Value::Int(1)]);
}

#[test]
fn bound_program_preserves_full_schema_overloads() {
    let int_function = HostFunctionSchema::with_return(
        "bound::overloaded",
        vec![HostParamSchema::value("value", HostTypeSchema::Int)],
        HostTypeSchema::Int,
    );
    let string_function = HostFunctionSchema::with_return(
        "bound::overloaded",
        vec![HostParamSchema::value("value", HostTypeSchema::String)],
        HostTypeSchema::String,
    );
    let mut catalog_builder = HostApiBuilder::new();
    catalog_builder.function(int_function.clone());
    catalog_builder.function(string_function.clone());
    let catalog = catalog_builder.build().expect("overload catalog");
    let int_schema = HostImportSchema::from_function(&catalog, &int_function);
    let string_schema = HostImportSchema::from_function(&catalog, &string_function);

    let mut bytecode = BytecodeBuilder::new();
    bytecode.ldc(0);
    bytecode.call(0, 1);
    bytecode.ldc(1);
    bytecode.call(1, 1);
    bytecode.ret();
    let program = Arc::new(
        Program::with_imports_and_debug(
            vec![Value::Int(1), Value::string("x")],
            bytecode.finish(),
            vec![
                HostImport {
                    name: "bound::overloaded".to_string(),
                    arity: 1,
                    return_type: vm::ValueType::Int,
                },
                HostImport {
                    name: "bound::overloaded".to_string(),
                    arity: 1,
                    return_type: vm::ValueType::String,
                },
            ],
            None,
        )
        .with_host_import_schemas(vec![int_schema.clone(), string_schema.clone()])
        .expect("aligned overload schemas"),
    );
    let mut registry = HostFunctionRegistry::empty();
    registry
        .register_catalog_static(int_schema, return_int)
        .expect("integer overload");
    registry
        .register_catalog_static(string_schema, |_vm, _args| {
            Ok(CallOutcome::Return(CallReturn::one(Value::string("text"))))
        })
        .expect("string overload");
    let bound = registry
        .bind_program_once(program)
        .expect("prepare overload binding");
    let mut vm = Vm::new_bound(bound).expect("construct overload vm");
    assert_eq!(vm.run().expect("run overload vm"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(7), Value::string("text")]);
}

#[test]
fn bound_program_rejects_registry_generation_changes() {
    let function =
        HostFunctionSchema::with_return("bound::generation", Vec::new(), HostTypeSchema::Int);
    let (import, schema) = schema_for(&function);
    let mut bytecode = BytecodeBuilder::new();
    bytecode.ret();
    let program = program_with_imports(vec![import], vec![schema.clone()], bytecode.finish());
    let mut registry = HostFunctionRegistry::empty();
    registry
        .register_catalog_static(schema, return_int)
        .expect("generation binding");
    let bound = registry
        .bind_program_once(program)
        .expect("prepare generation binding");

    registry.register_static("bound::after", 0, return_int);
    let error = match Vm::new_bound(bound) {
        Ok(_) => panic!("stale bound program must fail closed"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("stale"));
}

#[test]
fn bound_program_survives_a_failed_registry_transaction() {
    let function =
        HostFunctionSchema::with_return("bound::transaction", Vec::new(), HostTypeSchema::Int);
    let (import, schema) = schema_for(&function);
    let mut bytecode = BytecodeBuilder::new();
    bytecode.ret();
    let program = program_with_imports(vec![import], vec![schema.clone()], bytecode.finish());
    let mut registry = HostFunctionRegistry::empty();
    registry
        .register_catalog_static(schema, return_int)
        .expect("transaction binding");
    let bound = registry
        .bind_program_once(program)
        .expect("prepare transaction binding");

    let result: vm::VmResult<()> = registry.transactionally(|staged| {
        staged.register_static("bound::temporary", 0, return_int);
        Err(vm::VmError::HostError("abort transaction".to_string()))
    });
    assert!(result.is_err());
    Vm::new_bound(bound).expect("failed transaction must preserve the bound program");
}

#[test]
fn bound_program_rejects_capability_profile_changes() {
    let function =
        HostFunctionSchema::with_return("bound::capability", Vec::new(), HostTypeSchema::Int);
    let (import, _) = schema_for(&function);
    let mut bytecode = BytecodeBuilder::new();
    bytecode.ret();
    let program = Arc::new(Program::with_imports_and_debug(
        Vec::new(),
        bytecode.finish(),
        vec![import],
        None,
    ));
    let mut registry = HostFunctionRegistry::empty();
    registry.register_static("bound::capability", 0, return_int);
    let bound = registry
        .bind_program_once(program)
        .expect("prepare capability binding");

    registry
        .allow_builtin("bound::capability")
        .expect("registered host capability should be known");
    let error = match Vm::new_bound(bound) {
        Ok(_) => panic!("changed capability profile must fail closed"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("stale"));
}

#[test]
fn bound_program_rejects_catalog_schema_mismatch_during_prepare() {
    let registered = HostFunctionSchema::with_return(
        "bound::schema",
        vec![HostParamSchema::value("value", HostTypeSchema::Int)],
        HostTypeSchema::Int,
    );
    let requested = HostFunctionSchema::with_return(
        "bound::schema",
        vec![HostParamSchema::value("value", HostTypeSchema::String)],
        HostTypeSchema::Int,
    );
    let (import, registered_schema) = schema_for(&registered);
    let (_, requested_schema) = schema_for(&requested);
    let mut bytecode = BytecodeBuilder::new();
    bytecode.ret();
    let program = program_with_imports(vec![import], vec![requested_schema], bytecode.finish());
    let mut registry = HostFunctionRegistry::empty();
    registry
        .register_catalog_static(registered_schema, return_int)
        .expect("registered schema");

    let error = match registry.bind_program_once(program) {
        Ok(_) => panic!("catalog schema mismatch must fail closed"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("bound::schema"));
}

fn native_jit_supported() -> bool {
    (cfg!(target_arch = "x86_64")
        && (cfg!(target_os = "windows") || (cfg!(unix) && !cfg!(target_os = "macos"))))
        || (cfg!(target_arch = "aarch64")
            && (cfg!(target_os = "linux") || cfg!(target_os = "macos")))
}

#[test]
fn build_scanner_uses_the_shared_host_type_parser() {
    let function: syn::ItemFn = parse_quote! {
        fn inspect(
            #[pd_host_resource(passing = "take_owned")]
            resource: DemoResource,
            optional: Option<i64>,
        ) -> VmResult<Option<String>> {
            unimplemented!()
        }
    };
    let params = build_script::parse_callable_params(&function);
    assert_eq!(params.len(), 2);
    assert_eq!(params[0].ty_label, "resource");
    assert!(!params[0].optional);
    assert_eq!(params[1].ty_label, "int | null");
    assert!(params[1].optional);
    assert_eq!(
        pd_host_schema::type_label(&parse_quote!(VmResult<Option<String>>)).unwrap(),
        "string | null"
    );
}

#[test]
fn preserves_typed_callable_host_parameter_schema() {
    let ty: syn::Type = parse_quote!(VmCallable<fn(VmMap) -> VmMap>);
    assert_eq!(
        pd_host_schema::type_label(&ty).expect("callable type should parse"),
        "fn(map) -> map"
    );
    assert_eq!(
        callable_param_expr("fn(map) -> map"),
        "CallableParamType::Callable(CallableType { params: &[CallableParamType::Map], return_type: &CallableParamType::Map })"
    );

    let float_ty: syn::Type = parse_quote!(VmCallable<fn(f64) -> f64>);
    assert_eq!(
        pd_host_schema::type_label(&float_ty).expect("callable type should parse"),
        "fn(float) -> float"
    );
    assert_eq!(
        callable_param_expr("fn(float) -> float"),
        "CallableParamType::Callable(CallableType { params: &[CallableParamType::Float], return_type: &CallableParamType::Float })"
    );
}

#[test]
fn classifies_best_effort_host_bindings_from_signatures() {
    for function in [
        parse_quote!(
            fn host(vm: &mut Vm, value: i64) -> i64 {}
        ),
        parse_quote!(
            fn host(value: i64, vm: &mut (crate::vm::Vm)) -> VmResult<i64> {}
        ),
    ] {
        assert_eq!(
            classify_host_binding(&function),
            HostBindingKind::StaticStack
        );
    }

    for function in [
        parse_quote!(
            fn host() -> CallOutcome {}
        ),
        parse_quote!(
            fn host() -> VmResult<CallOutcome> {}
        ),
        parse_quote!(
            fn host() -> (VmResult<&CallOutcome>) {}
        ),
    ] {
        assert_eq!(
            classify_host_binding(&function),
            HostBindingKind::StaticArgs
        );
    }

    for function in [
        parse_quote!(
            fn host() {}
        ),
        parse_quote!(
            fn host() -> () {}
        ),
        parse_quote!(
            fn host() -> Option<i64> {}
        ),
        parse_quote!(
            fn host() -> bool {}
        ),
        parse_quote!(
            fn host() -> VmResult<bool> {}
        ),
        parse_quote!(
            fn host() -> Value {}
        ),
        parse_quote!(
            fn host() -> Vec<Value> {}
        ),
        parse_quote!(
            fn host() -> Vec<(Value, Value)> {}
        ),
        parse_quote!(
            fn host() -> SharedArray {}
        ),
        parse_quote!(
            fn host() -> NumberValue {}
        ),
    ] {
        assert_eq!(
            classify_host_binding(&function),
            HostBindingKind::StaticNonYieldingArgs
        );
    }

    for unsupported in [
        parse_quote!(
            fn host() -> impl IntoVmValue {}
        ),
        parse_quote!(
            fn host() -> VmResult {}
        ),
        parse_quote!(
            fn host() -> Result<bool, HostError> {}
        ),
        parse_quote!(
            fn host() -> CustomReturn {}
        ),
        parse_quote!(
            fn host() -> Vec<bool> {}
        ),
        parse_quote!(
            fn host() -> VmResult<Result<bool, HostError>> {}
        ),
        parse_quote!(
            fn host() -> Option<bool, i64> {}
        ),
    ] {
        assert_eq!(
            classify_host_binding(&unsupported),
            HostBindingKind::StaticArgs
        );
    }
}

#[test]
fn infers_host_suspension_from_the_return_signature() {
    for function in [
        parse_quote!(
            fn host() -> HostCallResult<Value> {}
        ),
        parse_quote!(
            fn host() -> VmResult<HostCallResult<Value>> {}
        ),
    ] {
        assert_eq!(
            infer_host_execution(&function),
            HostExecutionKind::MaySuspend
        );
        assert_eq!(
            classify_host_binding(&function),
            HostBindingKind::StaticArgs
        );
    }

    let synchronous = parse_quote!(
        fn host() -> VmResult<Value> {}
    );
    assert_eq!(infer_host_execution(&synchronous), HostExecutionKind::Sync);

    let asynchronous = parse_quote!(
        async fn host(value: String) -> VmResult<String> {}
    );
    assert_eq!(
        infer_host_execution(&asynchronous),
        HostExecutionKind::MaySuspend
    );
    assert_eq!(
        classify_host_binding(&asynchronous),
        HostBindingKind::StaticStack
    );
}

fn assert_runtime_sleep_loop_uses_native_host_call(bind_cached_registry: bool) {
    let compiled = compile_source(
        r#"
            use runtime;
            let mut i = 0;
            while i < 100 {
                let _ = runtime::sleep(0);
                i = i + 1;
            }
            i;
        "#,
    )
    .expect("runtime::sleep loop should compile");
    let mut vm = Vm::new(compiled.program);
    vm.set_jit_config(JitConfig {
        enabled: native_jit_supported(),
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
    if bind_cached_registry {
        HostFunctionRegistry::new()
            .bind_vm_cached(&mut vm)
            .expect("cached registry should bind runtime::sleep");
    }

    let status = vm.run();
    assert!(
        status.is_ok(),
        "runtime::sleep loop should run: {status:?}\n{}",
        vm.dump_jit_info()
    );
    assert_eq!(status.unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(100)]);

    if native_jit_supported() {
        let snapshot = vm.jit_snapshot();
        assert!(
            snapshot.traces.iter().any(|trace| {
                trace.terminal == JitTraceTerminal::LoopBack
                    && trace.op_names().iter().any(|op| op == "host_call")
                    && trace.ssa_text().contains("host_call")
            }),
            "runtime::sleep should remain in a loop-back trace, cached={bind_cached_registry}, dump:\n{}",
            vm.dump_jit_info()
        );
        assert!(
            vm.jit_native_exec_count() > 0,
            "runtime::sleep loop should execute natively, cached={bind_cached_registry}, dump:\n{}",
            vm.dump_jit_info()
        );
    }
}

#[test]
fn runtime_sleep_default_bindings_remain_inside_jit_loop_traces() {
    assert_runtime_sleep_loop_uses_native_host_call(false);
    assert_runtime_sleep_loop_uses_native_host_call(true);
}

#[test]
fn runtime_exit_still_halts_for_direct_and_cached_default_bindings() {
    for bind_cached_registry in [false, true] {
        let compiled = compile_source(
            r#"
                use runtime;
                runtime::exit();
                99;
            "#,
        )
        .expect("runtime::exit program should compile");
        let mut vm = Vm::new(compiled.program);
        if bind_cached_registry {
            HostFunctionRegistry::new()
                .bind_vm_cached(&mut vm)
                .expect("cached registry should bind runtime::exit");
        }

        assert_eq!(
            vm.run().expect("runtime::exit should run"),
            VmStatus::Halted
        );
        assert!(vm.stack().is_empty());
    }
}

#[test]
fn restricted_capabilities_disable_trace_jit_for_host_imports_and_builtins() {
    for source in [
        r#"
            use runtime;
            let mut i = 0;
            while i < 4 {
                let _ = runtime::sleep(0);
                i = i + 1;
            }
            i;
        "#,
        r#"
            use re;
            let mut i = 0;
            while i < 4 {
                let _ = re::match("a", "a");
                i = i + 1;
            }
            i;
        "#,
    ] {
        let compiled = compile_source(source).expect("restricted loop should compile");
        let mut vm = Vm::new(compiled.program);
        vm.set_jit_config(JitConfig {
            enabled: native_jit_supported(),
            hot_loop_threshold: 1,
            max_trace_len: 512,
        });
        let error = HostFunctionRegistry::restricted()
            .bind_vm_cached(&mut vm)
            .expect_err("restricted registry should reject ungranted capability during preflight");

        assert!(
            error
                .to_string()
                .contains("capability profile does not allow")
        );
        assert_eq!(vm.jit_native_exec_count(), 0);
    }
}

#[cfg(all(feature = "http-client", not(target_family = "wasm")))]
#[test]
fn generated_http_imports_are_unique_typed_and_independently_capability_gated() {
    const IMPORTS: [&str; 2] = ["http::client::request", "http::client::sse"];
    let callables = vm::default_host_callables();
    for name in IMPORTS {
        let discovered = callables
            .iter()
            .filter(|callable| callable.name == name)
            .collect::<Vec<_>>();
        assert_eq!(discovered.len(), 1, "{name} discovery count");
        let callable = discovered[0];
        assert_eq!(callable.signature.return_type, "map");
        if name == "http::client::request" {
            assert_eq!(callable.signature.params.len(), 1);
            assert_eq!(callable.signature.params[0].ty.display_label(), "map");
        } else {
            assert_eq!(callable.signature.params.len(), 2);
            assert_eq!(callable.signature.params[0].ty.display_label(), "map");
            assert_eq!(
                callable.signature.params[1].ty.display_label(),
                "fn(map) -> map"
            );
            assert_eq!(callable.host_execution, vm::HostExecution::MaySuspend);
        }
    }

    for mask in 0_u8..4 {
        let mut builder = CapabilityProfile::builder();
        for (index, name) in IMPORTS.iter().enumerate() {
            if mask & (1 << index) != 0 {
                builder = builder.allow_host_import(*name);
            }
        }
        let profile = builder.build();
        for (index, name) in IMPORTS.iter().enumerate() {
            assert_eq!(
                profile.allows_host_import(name),
                mask & (1 << index) != 0,
                "mask {mask:02b}, import {name}"
            );
        }
    }
}

#[test]
fn capability_profile_fingerprint_uses_stable_callable_identities() {
    let first = CapabilityProfile::builder()
        .allow_builtin(BuiltinFunction::JsonEncode)
        .allow_host_import("custom::echo")
        .build();
    let reordered = CapabilityProfile::builder()
        .allow_host_import("custom::echo")
        .allow_builtin(BuiltinFunction::JsonEncode)
        .build();

    assert_eq!(first, reordered);
    assert_eq!(first.fingerprint(), reordered.fingerprint());
    assert!(first.allows_builtin(BuiltinFunction::JsonEncode));
    assert!(first.allows_host_import("custom::echo"));
    assert!(!first.allows_host_import("custom::other"));
    assert_ne!(
        first.fingerprint(),
        CapabilityProfile::deny_all().fingerprint()
    );
    assert_ne!(
        CapabilityProfile::allow_all().fingerprint(),
        CapabilityProfile::deny_all().fingerprint()
    );
}

#[test]
fn vm_host_core_does_not_name_builtin_subsystem_policies() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let host_runtime = std::fs::read_to_string(manifest.join("src/vm/host_runtime.rs"))
        .expect("host runtime source");
    let capability =
        std::fs::read_to_string(manifest.join("src/vm/capability.rs")).expect("capability source");
    let host = std::fs::read_to_string(manifest.join("src/vm/host.rs")).expect("host source");

    for forbidden in [
        "HttpState",
        "IoPolicy",
        "SqlitePolicy",
        "http_state",
        "io_policy",
        "sqlite_policy",
    ] {
        assert!(
            !host_runtime.contains(forbidden),
            "HostRuntime leaked {forbidden}"
        );
        assert!(
            !capability.contains(forbidden),
            "capability.rs leaked {forbidden}"
        );
        assert!(!host.contains(forbidden), "host.rs leaked {forbidden}");
    }
}
