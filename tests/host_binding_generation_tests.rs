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
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
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
        let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
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
        let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
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
        let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
        let mut registry = HostFunctionRegistry::new();
        // The standard compile entry emits exact V13 imports, so register the
        // standard HTTP extension against the combined snapshot — the
        // capability profile gate is orthogonal to exact registration.
        vm::register_http_builtin_module(&mut registry)
            .expect("standard HTTP registration should succeed");
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

// ---- external host-extension SDK -------------------------------------------
//
// These tests exercise the public `vm::host_extension` surface exactly as an
// external host crate would (only public API): the catalog schema identity +
// fingerprint contract, the `HostExtension` register/install lifecycle and
// `Vm::install_extension`, restricted-registry capability gating, and the
// absence of raw-fingerprint / name-only-fallback escape hatches.

mod external_extension_sdk {
    use super::*;
    use std::sync::Arc;
    use vm::compiler::{CompileSourceFileOptions, SourceFlavor};
    use vm::host_extension::catalog_import_schemas;
    use vm::{
        CallOutcome, CallReturn, HostApiBuilder, HostApiCatalog, HostExtension, HostFunctionSchema,
        HostImportBindingError, HostParamSchema, HostTypeSchema, ResourceTypeKey,
        ResourceTypeSchema, VmError, VmResult, compile_source_with_flavor_and_options,
    };

    #[derive(Clone, Debug)]
    struct CounterPolicy {
        max: u64,
    }

    fn counter_catalog() -> Arc<HostApiCatalog> {
        let key = ResourceTypeKey::new("demo.counter").expect("valid key");
        let mut builder = HostApiBuilder::new();
        builder.resource(ResourceTypeSchema::new(
            key.clone(),
            "an external counter resource",
        ));
        builder.function(HostFunctionSchema::with_return(
            "demo::ping",
            Vec::new(),
            HostTypeSchema::Int,
        ));
        builder.function(HostFunctionSchema::with_return(
            "demo::make_counter",
            vec![HostParamSchema::value("seed", HostTypeSchema::Int)],
            HostTypeSchema::Resource(key),
        ));
        Arc::new(builder.build().expect("catalog must build"))
    }

    fn compile_with_catalog(catalog: &Arc<HostApiCatalog>, source: &str) -> vm::CompiledProgram {
        compile_source_with_flavor_and_options(
            source,
            SourceFlavor::RustScript,
            CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(catalog)),
        )
        .expect("catalog source should compile")
    }

    fn ping_adapter(_vm: &mut vm::Vm, _args: &[Value]) -> vm::VmResult<CallOutcome> {
        Ok(CallOutcome::Return(CallReturn::One(Value::Int(11))))
    }

    /// An external-style extension using the public `HostExtension` lifecycle.
    struct CounterExtension;

    impl vm::HostExtension for CounterExtension {
        fn register(&self, registry: &mut HostFunctionRegistry) -> vm::VmResult<()> {
            let catalog = counter_catalog();
            for schema in catalog_import_schemas(&catalog, "demo::ping") {
                registry.register_exact_static("demo::ping", 0, schema, ping_adapter)?;
            }
            Ok(())
        }

        fn install(&self, vm: &mut Vm) {
            vm.host_context().set_module_state(CounterPolicy { max: 7 });
        }
    }

    #[test]
    fn catalog_schema_identity_matches_the_compiler_embedded_schema() {
        let catalog = counter_catalog();
        // The scalar function: the public adapter's schema must be
        // byte-for-byte the schema the compiler embeds at the call site
        // (labels, schemas, passing, and the catalog fingerprint).
        let compiled = compile_with_catalog(&catalog, "use demo;\ndemo::ping();\n");
        let ping_import = compiled
            .program
            .imports
            .iter()
            .find(|import| import.name == "demo::ping")
            .expect("ping import")
            .schema
            .clone()
            .expect("exact schema");
        let schemas = catalog_import_schemas(&catalog, "demo::ping");
        assert_eq!(
            schemas.len(),
            1,
            "one declared ping overload maps to exactly one exact schema"
        );
        assert_eq!(
            schemas[0], ping_import,
            "registration schema must be identical to the compiler-embedded schema"
        );

        // The resource-bearing function likewise preserves the resource key.
        let compiled = compile_with_catalog(&catalog, "use demo;\ndemo::make_counter(3);\n");
        let make_import = compiled
            .program
            .imports
            .iter()
            .find(|import| import.name == "demo::make_counter")
            .expect("make import")
            .schema
            .clone()
            .expect("exact schema");
        let schemas = catalog_import_schemas(&catalog, "demo::make_counter");
        assert_eq!(schemas.len(), 1);
        assert_eq!(
            schemas[0], make_import,
            "resource-returning schema must preserve the ResourceTypeKey and fingerprint"
        );
        assert_eq!(
            schemas[0].return_type,
            compile_type_schema_resource(),
            "the catalog resource maps onto the nominal TypeSchema::Resource"
        );
    }

    fn compile_type_schema_resource() -> vm::compiler::TypeSchema {
        let key = ResourceTypeKey::new("demo.counter").expect("valid key");
        vm::compiler::TypeSchema::Resource(key)
    }

    #[test]
    fn unknown_function_produces_no_schema_and_exact_resolution_refuses_name_fallback() {
        let catalog = counter_catalog();
        // No overloads -> no schema: an unknown name can never be synthesized.
        assert!(
            catalog_import_schemas(&catalog, "demo::missing").is_empty(),
            "an undeclared name must produce no exact schema (no name-only fallback)"
        );

        // And the registry rejects a schema-less resolution for that name with
        // a structured MissingExact error rather than matching by name.
        let registry = HostFunctionRegistry::new();
        let import = vm::HostImport {
            name: "demo::missing".into(),
            arity: 0,
            return_type: vm::ValueType::Int,
            schema: Some(vm::HostImportSchema {
                params: Vec::new(),
                return_type: vm::compiler::TypeSchema::Int,
                fingerprint: catalog.fingerprint(),
            }),
        };
        let error = registry
            .resolve_import(&import)
            .expect_err("an unregistered exact import must be rejected");
        assert!(matches!(
            error,
            VmError::HostImportBinding(HostImportBindingError::MissingExact { .. })
        ));
    }

    #[test]
    fn install_extension_registers_functions_and_persistent_module_state() {
        let catalog = counter_catalog();
        let compiled = compile_with_catalog(&catalog, "use demo;\ndemo::ping();\n");
        let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
        vm.install_extension(&CounterExtension)
            .expect("extension should install");

        let policy = {
            let context = vm.host_context();
            context
                .module_state::<CounterPolicy>()
                .expect("installed module state")
                .max
        };
        assert_eq!(
            policy, 7,
            "module state is set through HostExtension::install"
        );
        assert_eq!(vm.run().expect("run"), VmStatus::Halted);
        assert_eq!(
            vm.stack(),
            &[Value::Int(11)],
            "the registered exact host function answers through the script"
        );
    }

    #[test]
    fn restricted_registry_requires_an_explicit_grant_for_external_exact_imports() {
        let catalog = counter_catalog();
        let compiled = compile_with_catalog(&catalog, "use demo;\ndemo::ping();\n");
        let mut registry = HostFunctionRegistry::restricted();
        CounterExtension
            .register(&mut registry)
            .expect("extension registration must succeed on a restricted registry");

        let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
        let error = registry
            .bind_vm_cached(&mut vm)
            .expect_err("ungranted external import must be rejected");
        assert!(
            error.to_string().contains("capability profile"),
            "missing grant must surface the capability-profile rejection: {error}"
        );

        // Explicitly granting the import binds and runs.
        let compiled = compile_with_catalog(&catalog, "use demo;\ndemo::ping();\n");
        let mut granted = HostFunctionRegistry::restricted();
        CounterExtension
            .register(&mut granted)
            .expect("register on restricted registry");
        let profile = CapabilityProfile::builder()
            .allow_host_import("demo::ping")
            .build();
        granted.set_capability_profile(profile);
        let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
        granted
            .bind_vm_cached(&mut vm)
            .expect("granted external import must bind");
        assert_eq!(vm.run().expect("run"), VmStatus::Halted);
        assert_eq!(vm.stack(), &[Value::Int(11)]);
    }

    /// An extension whose `register` fails part-way (a duplicate exact schema)
    /// to exercise `install_extension` transactional failure semantics.
    struct DuplicateNameExtension;

    impl HostExtension for DuplicateNameExtension {
        fn register(&self, registry: &mut HostFunctionRegistry) -> VmResult<()> {
            let catalog = counter_catalog();
            // Register `demo::ping` twice with the identical exact schema and
            // arity; the second registration is rejected as a duplicate.
            for schema in catalog_import_schemas(&catalog, "demo::ping") {
                registry.register_exact_static("demo::ping", 0, schema.clone(), ping_adapter)?;
                registry.register_exact_static("demo::ping", 0, schema, ping_adapter)?;
            }
            Ok(())
        }

        fn install(&self, _vm: &mut Vm) {
            // Never reached on the failing path; present to prove install is
            // also skipped on register failure.
            unreachable!("register failure must abort before install");
        }
    }

    #[test]
    fn install_extension_register_failure_is_transactional_and_retryable() {
        let catalog = counter_catalog();
        let compiled = compile_with_catalog(&catalog, "use demo;\ndemo::ping();\n");
        let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");

        // A registration failure (duplicate exact schema) fails the whole
        // install before any install mutation happens...
        let error = vm
            .install_extension(&DuplicateNameExtension)
            .expect_err("duplicate registration must fail install_extension");
        assert!(
            matches!(
                error,
                VmError::HostImportBinding(HostImportBindingError::Duplicate { .. })
            ),
            "expected a structured duplicate error, got {error}"
        );

        // ...so the VM is left unbound with no module state, and a corrected
        // extension installs cleanly on the same VM (retry/recovery).
        assert!(
            vm.host_context().is_module_state_empty(),
            "a failed install must not leave module state behind"
        );
        vm.install_extension(&CounterExtension)
            .expect("retrying with a valid extension must succeed on the same VM");
        assert_eq!(
            vm.host_context()
                .module_state::<CounterPolicy>()
                .map(|policy| policy.max),
            Some(7),
            "the retried install installs its module state"
        );
        assert_eq!(vm.run().expect("run"), VmStatus::Halted);
        assert_eq!(vm.stack(), &[Value::Int(11)]);
    }

    #[derive(Debug)]
    struct SecondPolicy {
        // Never installed: the binding-failure test asserts this extension's
        // state is *not* written, so the payload is intentionally unread.
        #[allow(dead_code)]
        max: u64,
    }

    /// A second extension that registers the same exact function as
    /// `CounterExtension` but installs a distinct module-state type. Installing
    /// it on an already-bound VM fails specifically at binding.
    struct SecondPolicyExtension;

    impl HostExtension for SecondPolicyExtension {
        fn register(&self, registry: &mut HostFunctionRegistry) -> VmResult<()> {
            let catalog = counter_catalog();
            for schema in catalog_import_schemas(&catalog, "demo::ping") {
                registry.register_exact_static("demo::ping", 0, schema, ping_adapter)?;
            }
            Ok(())
        }

        fn install(&self, vm: &mut Vm) {
            vm.host_context().set_module_state(SecondPolicy { max: 99 });
        }
    }

    #[test]
    fn install_extension_binding_failure_leaves_first_extension_intact() {
        let catalog = counter_catalog();
        let compiled = compile_with_catalog(&catalog, "use demo;\ndemo::ping();\n");
        let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");

        vm.install_extension(&CounterExtension)
            .expect("first install binds");
        assert_eq!(vm.run().expect("run"), VmStatus::Halted);
        assert_eq!(vm.stack(), &[Value::Int(11)]);

        // A second install on the already-bound VM fails at binding — before
        // the second extension's module state could be installed.
        let error = vm
            .install_extension(&SecondPolicyExtension)
            .expect_err("binding an already-bound VM must fail");
        assert!(
            error.to_string().contains("unbound vm"),
            "expected the binding rejection, got {error}"
        );

        // The failed second install installed no module state of its own and
        // left the first extension's binding + module state fully intact.
        assert!(
            vm.host_context().module_state::<SecondPolicy>().is_none(),
            "a failure at binding must happen before the second install mutation"
        );
        assert_eq!(
            vm.host_context()
                .module_state::<CounterPolicy>()
                .map(|policy| policy.max),
            Some(7),
            "the first extension's module state survives the failed second install"
        );
        vm.reset_for_reuse();
        assert_eq!(
            vm.run().expect("second run after reset"),
            VmStatus::Halted,
            "the first extension's binding still executes after the failed second install"
        );
        assert_eq!(vm.stack(), &[Value::Int(11)]);
    }

    /// The public extension surface must not leak a raw fingerprint
    /// constructor or a name-only registration path (arch boundary).
    #[test]
    fn extension_surface_exposes_no_raw_fingerprint_or_name_only_fallback() {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let extension = std::fs::read_to_string(manifest.join("src/vm/host_extension.rs"))
            .expect("host_extension source");

        // The only fingerprint entry point documented/exported is the catalog's
        // own `fingerprint()`; the module must not construct one from raw bits.
        assert!(
            !extension.contains("HostApiFingerprint("),
            "host_extension must not construct a raw HostApiFingerprint"
        );
        // The module is host-agnostic: no builtin or SQLite coupling.
        for forbidden in [
            "crate::builtins",
            "rusqlite",
            "Sqlite",
            "HttpState",
            "IoPolicy",
        ] {
            assert!(
                !extension.contains(forbidden),
                "host_extension leaked {forbidden}"
            );
        }
    }
}
