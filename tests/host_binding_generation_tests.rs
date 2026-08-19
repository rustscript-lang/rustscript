#[allow(dead_code)]
#[path = "../build.rs"]
mod build_script;

use build_script::{
    HostBindingKind, HostExecutionKind, callable_param_expr, classify_host_binding,
    infer_host_execution, type_label,
};
use syn::parse_quote;
use vm::{
    BuiltinFunction, CapabilityProfile, HostFunctionRegistry, JitConfig, JitTraceTerminal, Value,
    Vm, VmStatus, compile_source,
};
#[cfg(feature = "http-client")]
use vm::{HostExecution, default_host_callables};

fn native_jit_supported() -> bool {
    (cfg!(target_arch = "x86_64")
        && (cfg!(target_os = "windows") || (cfg!(unix) && !cfg!(target_os = "macos"))))
        || (cfg!(target_arch = "aarch64")
            && (cfg!(target_os = "linux") || cfg!(target_os = "macos")))
}

#[test]
fn preserves_typed_callable_host_parameter_schema() {
    let ty: syn::Type = parse_quote!(VmCallable<fn(VmMap) -> VmMap>);
    assert_eq!(type_label(&ty), "fn(map) -> map");
    assert_eq!(
        callable_param_expr("fn(map) -> map"),
        "CallableParamType::Callable(CallableType { params: &[CallableParamType::Map], return_type: &CallableParamType::Map })"
    );

    let float_ty: syn::Type = parse_quote!(VmCallable<fn(f64) -> f64>);
    assert_eq!(type_label(&float_ty), "fn(float) -> float");
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

    let synchronous = parse_quote!(
        fn host() -> VmResult<Value> {}
    );
    assert_eq!(infer_host_execution(&synchronous), HostExecutionKind::Sync);
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

#[cfg(feature = "http-client")]
#[test]
fn generated_http_imports_are_unique_typed_and_independently_capability_gated() {
    const IMPORTS: [&str; 2] = ["http::client::request", "http::client::sse"];
    let callables = default_host_callables();
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
            assert_eq!(callable.host_execution, HostExecution::MaySuspend);
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

        let source = r#"
            use http;
            fn callback(item: map) -> map { { action: "stop" } }
            http::client::request({ url: "https://example.test/" });
            http::client::sse({ url: "https://example.test/" }, callback);
        "#;
        let compiled = compile_source(source).expect("HTTP imports should compile");
        let mut vm = Vm::new(compiled.program);
        let mut registry = HostFunctionRegistry::new();
        registry.set_capability_profile(profile);
        let result = registry.bind_vm_cached(&mut vm);
        if mask == 0b11 {
            result.expect("both explicit capabilities should bind");
        } else {
            let error = result.expect_err("a missing HTTP capability must reject binding");
            assert!(error.to_string().contains("capability profile"), "{error}");
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
            "CapabilityProfile leaked {forbidden}"
        );
    }
    for forbidden in ["configure_http", "configure_sqlite", "http_is_configured"] {
        assert!(!host.contains(forbidden), "Vm API leaked {forbidden}");
    }
}

// ---- resource catalog scanner -----------------------------------------------
//
// These tests run the *real* build-script scanner (`parse_callable_params`,
// `type_label` — the same functions build.rs uses to build the published
// catalog) over resource-bearing `pd_host_function` signatures and check that
// the ordered label/schema/passing descriptor it computes is byte-for-byte the
// one the shared `pd-host-schema` rules give the proc macro. The same fixtures
// are exercised on the macro side by `pd-host-function`'s own unit tests and by
// `tests/host_resource_macro_tests.rs`, so the two expansion paths cannot
// drift.

mod resource_catalog_scanner {
    use super::*;
    use build_script::{parse_callable_params, type_label};
    use pd_host_schema::{HostPassing, RESOURCE_SCHEMA_LABEL, resource_spec};
    use syn::FnArg;

    fn canonical_inputs<'a>(function: &'a syn::ItemFn) -> impl Iterator<Item = &'a syn::PatType> {
        function.sig.inputs.iter().filter_map(|input| match input {
            FnArg::Typed(pat_type) => Some(pat_type),
            FnArg::Receiver(_) => None,
        })
    }

    #[test]
    fn build_scanner_descriptor_matches_shared_proc_macro_resource_rules() {
        let fixtures: Vec<syn::ItemFn> = vec![
            parse_quote!(
                #[pd_host_function(name = "test::a")]
                /// A prefix ordinary argument before a borrowed resource.
                fn f(prefix: i64, r: ResourceRef<'_, FakeResource>) -> i64 {
                    todo!()
                }
            ),
            parse_quote!(
                #[pd_host_function(name = "test::b")]
                /// Ordinary and resource arguments interleaved.
                fn f(
                    prefix: i64,
                    r: ResourceMut<'_, FakeResource>,
                    n: i64,
                    m: ResourceOwned<FakeResource>,
                ) -> i64 {
                    todo!()
                }
            ),
            parse_quote!(
                #[pd_host_function(name = "test::c")]
                /// An explicitly annotated resource parameter.
                fn f(
                    #[pd_host_param(passing = "take_owned", key = "test.fake")] r: FakeResource,
                    n: i64,
                ) -> i64 {
                    todo!()
                }
            ),
        ];

        for fixture in &fixtures {
            let scanned = parse_callable_params(fixture);
            let mut scanned = scanned.iter();
            for pat_type in canonical_inputs(fixture) {
                let Some(build_param) = scanned.next() else {
                    panic!("scanner produced fewer parameters than the signature");
                };
                match resource_spec(&pat_type.ty, &pat_type.attrs) {
                    Ok(Some(spec)) => {
                        assert_eq!(
                            build_param.ty_label, RESOURCE_SCHEMA_LABEL,
                            "resource schema label must match the proc macro"
                        );
                        assert_eq!(
                            build_param.passing,
                            spec.mode.host_passing(),
                            "resource passing must match the proc macro for '{}'",
                            build_param.name
                        );
                        assert_eq!(
                            build_param.resource_key.as_deref(),
                            spec.key.as_deref(),
                            "resource key must match the proc macro"
                        );
                    }
                    Ok(None) => {
                        assert_eq!(
                            build_param.passing,
                            HostPassing::Value,
                            "ordinary parameter passing must stay Value"
                        );
                    }
                    Err(message) => panic!("fixture must parse cleanly: {message}"),
                }
            }
            assert!(
                scanned.next().is_none(),
                "scanner produced more parameters than the signature"
            );
        }
    }

    #[test]
    fn resource_returns_get_the_resource_label_discoverable_by_the_scanner() {
        let owned: syn::Type = parse_quote!(Resource<FakeResource>);
        assert_eq!(type_label(&owned), RESOURCE_SCHEMA_LABEL);
        let borrowed: syn::Type = parse_quote!(ResourceRef<'_, FakeResource>);
        assert_eq!(type_label(&borrowed), RESOURCE_SCHEMA_LABEL);
        let mutable: syn::Type = parse_quote!(ResourceMut<'_, FakeResource>);
        assert_eq!(type_label(&mutable), RESOURCE_SCHEMA_LABEL);

        let result = std::panic::catch_unwind(|| {
            let input_only: syn::Type = parse_quote!(ResourceOwned<FakeResource>);
            type_label(&input_only)
        });
        assert!(
            result.is_err(),
            "ResourceOwned is an input-only TakeOwned wrapper"
        );
    }

    #[test]
    fn shared_key_validation_agrees_with_runtime_resource_type_key() {
        use pd_host_schema::validate_resource_key;
        let cases: &[&str] = &[
            "io.file",
            "file",
            "a-b.c_0",
            "0host",
            "",
            "io..file",
            ".x",
            "x.",
            "A.b",
            "bad key",
            "very_long_namespace.",
        ];
        for case in cases {
            let shared = validate_resource_key(case);
            let runtime = vm::ResourceTypeKey::new(*case).map(|_| ());
            match (shared, runtime) {
                (Ok(()), Ok(())) => {}
                (Err(_), Err(_)) => {}
                (Ok(()), Err(error)) => {
                    panic!("shared accepts but runtime rejects {case:?}: {error}")
                }
                (Err(error), Ok(())) => {
                    panic!("runtime accepts but shared rejects {case:?}: {error}")
                }
            }
        }
    }

    #[test]
    fn invalid_resource_keys_fail_the_build_scanner_at_build_time() {
        let fixture: syn::ItemFn = parse_quote!(
            #[pd_host_function(name = "test::bad")]
            /// An invalid explicit key must fail the build scanner.
            fn f(#[pd_host_param(passing = "take_owned", key = "bad key")] r: FakeResource) -> i64 {
                todo!()
            }
        );
        let result = std::panic::catch_unwind(|| parse_callable_params(&fixture));
        assert!(
            result.is_err(),
            "an invalid resource key must fail at build time, not at runtime"
        );
    }
}
