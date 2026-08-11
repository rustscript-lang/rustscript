#[allow(dead_code)]
#[path = "../build.rs"]
mod build_script;

use build_script::{
    HostBindingKind, HostExecutionKind, classify_host_binding, infer_host_execution,
};
use syn::parse_quote;
use vm::{
    BuiltinFunction, CapabilityProfile, HostFunctionRegistry, JitConfig, JitTraceTerminal, Value,
    Vm, VmStatus, compile_source,
};

fn native_jit_supported() -> bool {
    (cfg!(target_arch = "x86_64")
        && (cfg!(target_os = "windows") || (cfg!(unix) && !cfg!(target_os = "macos"))))
        || (cfg!(target_arch = "aarch64")
            && (cfg!(target_os = "linux") || cfg!(target_os = "macos")))
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
fn capability_profile_fingerprint_covers_http_policy() {
    let first_policy = vm::HttpConfig {
        allowed_hosts: vec!["example.com".to_string()],
        max_redirects: 1,
        ..vm::HttpConfig::default()
    };
    let second_policy = vm::HttpConfig {
        allowed_hosts: vec!["example.com".to_string()],
        max_redirects: 2,
        ..vm::HttpConfig::default()
    };
    let first = CapabilityProfile::builder()
        .http_policy(first_policy)
        .build();
    let second = CapabilityProfile::builder()
        .http_policy(second_policy)
        .build();

    assert_eq!(first.http_policy().expect("HTTP policy").max_redirects, 1);
    assert_ne!(first.fingerprint(), second.fingerprint());
}

#[test]
fn capability_profile_fingerprint_covers_io_policy() {
    let first = CapabilityProfile::builder()
        .io_policy(vm::IoPolicy {
            allowed_roots: vec!["/tmp/b".to_string(), "/tmp/a".to_string()],
            max_read_bytes: 10,
            ..vm::IoPolicy::default()
        })
        .build();
    let reordered = CapabilityProfile::builder()
        .io_policy(vm::IoPolicy {
            allowed_roots: vec!["/tmp/a".to_string(), "/tmp/b".to_string()],
            max_read_bytes: 10,
            ..vm::IoPolicy::default()
        })
        .build();
    let changed = CapabilityProfile::builder()
        .io_policy(vm::IoPolicy {
            allowed_roots: vec!["/tmp/a".to_string(), "/tmp/b".to_string()],
            max_read_bytes: 11,
            ..vm::IoPolicy::default()
        })
        .build();

    assert_eq!(first, reordered);
    assert_eq!(first.fingerprint(), reordered.fingerprint());
    assert_ne!(first.fingerprint(), changed.fingerprint());
}

#[cfg(feature = "sqlite")]
#[test]
fn capability_profile_fingerprint_covers_sqlite_policy() {
    let mut first_policy = vm::SqlitePolicy::default();
    first_policy.limits.max_rows = 10;
    let mut second_policy = first_policy.clone();
    second_policy.limits.max_rows = 11;
    let first = CapabilityProfile::builder()
        .sqlite_policy(first_policy)
        .build();
    let second = CapabilityProfile::builder()
        .sqlite_policy(second_policy)
        .build();

    assert_eq!(
        first
            .sqlite_policy()
            .expect("SQLite policy")
            .limits
            .max_rows,
        10
    );
    assert_ne!(first.fingerprint(), second.fingerprint());
}
