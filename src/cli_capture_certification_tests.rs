use super::*;
use crate::ResourceTypeKey;
use crate::resource::{HostResource, ResourceHandle};
use crate::{
    CaptureQueryError, CaptureRootState, HostApiBuilder, HostFunctionSchema, HostParamPassing,
    HostParamSchema, HostTypeSchema, ResourceTypeSchema,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct Payload(i64);
impl HostResource for Payload {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(key())
    }
}
fn key() -> ResourceTypeKey {
    ResourceTypeKey::new("neutral.cli-cell").unwrap()
}
#[derive(Default)]
struct Observations {
    init_certified: Vec<bool>,
    thresholds: Vec<u32>,
    fuels: Vec<Option<u64>>,
    depths: Vec<usize>,
    deadlines: Vec<Option<u64>>,
    calls: usize,
    live: usize,
    unavailable: usize,
    query_certified: Vec<bool>,
}
struct Probe {
    observations: Arc<Mutex<Observations>>,
    handle: Option<ResourceHandle>,
}
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = root.join(format!(
            "neutral-cli-capture-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn source(&self) -> PathBuf {
        let path = self.0.join("owner.rss");
        std::fs::write(&path, "let owner = neutral::open(); let dispatch = || if true => { neutral::check(); 41 } else => { neutral::close(owner); 41 }; dispatch();\n").unwrap();
        path
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn catalog() -> Arc<crate::HostApiCatalog> {
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(key(), "neutral CLI payload"));
    builder.function(HostFunctionSchema::with_return(
        "neutral::open",
        Vec::<HostParamSchema>::new(),
        HostTypeSchema::Resource(key()),
    ));
    builder.function(HostFunctionSchema::with_return(
        "neutral::check",
        Vec::<HostParamSchema>::new(),
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "neutral::close",
        vec![HostParamSchema::with_passing(
            "owner",
            HostTypeSchema::Resource(key()),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::Null,
    ));
    Arc::new(builder.build().unwrap())
}
fn services(observations: Arc<Mutex<Observations>>) -> CliHostServices<'static> {
    let catalog = catalog();
    let registered = Arc::clone(&catalog);
    CliHostServices::new(catalog, move || {
        let mut registry = HostFunctionRegistry::new();
        for schema in crate::catalog_import_schemas(&registered, "neutral::open") {
            registry
                .register_exact_static("neutral::open", 0, schema, |vm, _| {
                    let token = vm.host_context().push_resource(Payload(41)).unwrap();
                    let handle = token.handle();
                    vm.host_context()
                        .module_state_mut::<Probe>()
                        .unwrap()
                        .handle = Some(handle);
                    Ok(CallOutcome::Return(CallReturn::one(Value::Int(
                        handle.raw() as i64,
                    ))))
                })
                .unwrap();
        }
        for schema in crate::catalog_import_schemas(&registered, "neutral::check") {
            registry
                .register_exact_static("neutral::check", 0, schema, |vm, _| {
                    let certified_now = vm
                        .capture_certification_stats()
                        .is_some_and(|stats| stats.valid);
                    let handle = vm
                        .host_context()
                        .module_state::<Probe>()
                        .unwrap()
                        .handle
                        .unwrap();
                    let context = vm.host_context();
                    let candidate = context
                        .typed_resource_with_key::<Payload>(handle, &key())
                        .unwrap();
                    assert_eq!(context.resource(&candidate).unwrap().get().0, 41);
                    let live = match context.checked_move_resource_capture::<Payload>(&key()) {
                        Ok(checked) => {
                            assert_eq!(checked.token().handle(), candidate.handle());
                            assert_eq!(checked.root().observe(&context), CaptureRootState::Live);
                            true
                        }
                        Err(CaptureQueryError::Unavailable) => false,
                        Err(other) => panic!("unexpected checked capture outcome: {other:?}"),
                    };
                    let observations =
                        Arc::clone(&context.module_state::<Probe>().unwrap().observations);
                    let mut state = observations.lock().unwrap();
                    state.calls += 1;
                    state.query_certified.push(certified_now);
                    if live {
                        state.live += 1;
                    } else {
                        state.unavailable += 1;
                    }
                    Ok(CallOutcome::Return(CallReturn::one(Value::Int(41))))
                })
                .unwrap();
        }
        for schema in crate::catalog_import_schemas(&registered, "neutral::close") {
            registry
                .register_exact_static("neutral::close", 1, schema, |vm, args| {
                    let Value::Int(raw) = args[0] else {
                        panic!("typed carrier");
                    };
                    vm.host_context()
                        .close_resource::<Payload>(
                            ResourceHandle::from_raw(raw as u64).unwrap(),
                            crate::resource::ResourceCloseReason::Requested,
                        )
                        .unwrap();
                    Ok(CallOutcome::Return(CallReturn::none()))
                })
                .unwrap();
        }
        Ok(registry)
    })
    .with_vm_init(move |vm| {
        let certified = vm
            .capture_certification_stats()
            .is_some_and(|stats| stats.valid);
        let threshold = vm.jit_config().hot_loop_threshold;
        let fuel = vm.get_fuel();
        {
            let mut state = observations.lock().unwrap();
            state.init_certified.push(certified);
            state.thresholds.push(threshold);
            state.fuels.push(fuel);
            state.depths.push(vm.max_script_call_depth());
            state.deadlines.push(vm.epoch_deadline());
        }
        vm.host_context().set_module_state(Probe {
            observations: Arc::clone(&observations),
            handle: None,
        });
        Ok(())
    })
}
struct NeutralDialect;
impl crate::ParserDialect for NeutralDialect {}
fn parse_neutral(
    source: &str,
    context: &crate::SourceFrontendContext<'_>,
) -> Result<crate::FrontendIr, crate::ParseError> {
    let mut ir = crate::parse_source_with_dialect(
        source,
        &NeutralDialect,
        crate::SharedParserOptions {
            source_id: context.source_id,
            allow_implicit_externs: true,
            import_scan_mode: true,
            enforce_mutable_bindings: true,
            ..Default::default()
        },
    )?;
    let placeholders = ir
        .functions
        .iter()
        .filter(|f| {
            ir.implicit_extern_names.contains(&f.name)
                && matches!(f.name.as_str(), "open" | "close" | "check")
        })
        .cloned()
        .collect::<Vec<_>>();
    ir.functions
        .retain(|f| !placeholders.iter().any(|p| p.index == f.index));
    ir.implicit_extern_names
        .retain(|name| !placeholders.iter().any(|p| &p.name == name));
    let mut mapping = std::collections::HashMap::new();
    for old in placeholders {
        mapping.insert(
            old.index,
            context.install_host_function(&mut ir, &format!("neutral::{}", old.name), old.arity)?,
        );
    }
    fn remap(expr: &mut crate::Expr, mapping: &std::collections::HashMap<u16, u16>) {
        if let crate::Expr::Call(index, _, arguments, ..) = expr {
            if let Some(new) = mapping.get(index) {
                *index = *new;
            }
            for arg in arguments {
                remap(arg, mapping);
            }
        }
        match expr {
            crate::Expr::IfElse {
                condition,
                then_expr,
                else_expr,
            } => {
                remap(condition, mapping);
                remap(then_expr, mapping);
                remap(else_expr, mapping);
            }
            crate::Expr::Closure(closure) | crate::Expr::ClosureCall(closure, _) => {
                remap(&mut closure.body, mapping);
            }
            crate::Expr::Block { stmts, expr } => {
                for stmt in stmts {
                    if let crate::Stmt::Let { expr, .. } | crate::Stmt::Expr { expr, .. } = stmt {
                        remap(expr, mapping);
                    }
                }
                remap(expr, mapping);
            }
            _ => {}
        }
    }
    for statement in &mut ir.stmts {
        if let crate::Stmt::Let { expr, .. } | crate::Stmt::Expr { expr, .. } = statement {
            remap(expr, &mapping);
        }
    }
    for implementation in ir.function_impls.values_mut() {
        for statement in &mut implementation.body_stmts {
            if let crate::Stmt::Let { expr, .. } | crate::Stmt::Expr { expr, .. } = statement {
                remap(expr, &mapping);
            }
        }
        remap(&mut implementation.body_expr, &mapping);
    }
    Ok(ir)
}

struct CliPlugin {
    id: &'static str,
    extension: &'static [&'static str],
    ownership: bool,
}
impl crate::SourcePlugin for CliPlugin {
    fn descriptor(&self) -> crate::SourceFrontendDescriptor {
        crate::SourceFrontendDescriptor {
            id: crate::FrontendId::new(self.id),
            extensions: self.extension,
            typing_policy: crate::FrontendTypingPolicy::Strict,
            supports_ownership_checking: self.ownership,
        }
    }
    fn parse_source(
        &self,
        source: &str,
        context: &crate::SourceFrontendContext<'_>,
    ) -> Result<crate::FrontendIr, crate::ParseError> {
        parse_neutral(source, context)
    }
}
static CLI_PLUGIN_A: CliPlugin = CliPlugin {
    id: "neutral-cli-owner-a",
    extension: &["cap-a"],
    ownership: true,
};
static CLI_PLUGIN_B: CliPlugin = CliPlugin {
    id: "neutral-cli-owner-b",
    extension: &["cap-b"],
    ownership: false,
};
fn plugin_options_a() -> CompileSourceFileOptions {
    CompileSourceFileOptions::new()
        .with_source_plugin(&CLI_PLUGIN_A)
        .with_source_plugin(&CLI_PLUGIN_B)
        .with_module_override_path("dep.rss", "dep.cap-a")
}
fn plugin_options_b() -> CompileSourceFileOptions {
    CompileSourceFileOptions::new()
        .with_source_plugin(&CLI_PLUGIN_A)
        .with_source_plugin(&CLI_PLUGIN_B)
        .with_module_override_path("dep.rss", "dep.cap-b")
}
#[test]
fn linked_cli_uses_actual_plugin_producer_policy_not_entry_policy() {
    for (extension, options, expected_live) in [
        (
            "cap-a",
            plugin_options_a as fn() -> CompileSourceFileOptions,
            1,
        ),
        (
            "cap-b",
            plugin_options_b as fn() -> CompileSourceFileOptions,
            0,
        ),
    ] {
        let scratch = Scratch::new();
        let dependency = scratch.0.join(format!("dep.{extension}"));
        let source = scratch.0.join("main.rss");
        std::fs::write(
            &dependency,
            "let owner=open(); pub fn dispatch()->int {check();close(owner);41}",
        )
        .unwrap();
        std::fs::write(&source, "use self::dep; dep::dispatch();").unwrap();
        let observations = Arc::new(Mutex::new(Observations::default()));
        let selected = services(Arc::clone(&observations)).with_capture_certification();
        let runtime = CliRuntime {
            compile_options: options,
            ..CliRuntime::default()
        };
        run_with_args(&runtime, &[source.display().to_string()], &selected).unwrap();
        let state = observations.lock().unwrap();
        assert_eq!(state.init_certified, [true]);
        assert_eq!(
            (state.calls, state.live, state.unavailable),
            (1, expected_live, 1 - expected_live)
        );
    }
}

#[test]
fn opted_in_debug_keeps_issued_owner_and_configured_print_sink() {
    let scratch = Scratch::new();
    let source = scratch.source();
    let original = std::fs::read_to_string(&source).unwrap();
    std::fs::write(&source, format!("{original} print(41);")).unwrap();
    let observations = Arc::new(Mutex::new(Observations::default()));
    run_with_args(
        &CliRuntime::default(),
        &[
            "--debug".into(),
            "--no-stop-on-entry".into(),
            "--fuel".into(),
            "100000".into(),
            source.display().to_string(),
        ],
        &services(Arc::clone(&observations)).with_capture_certification(),
    )
    .unwrap();
    let state = observations.lock().unwrap();
    assert_eq!(state.init_certified, [true]);
    assert_eq!(state.live, 1);
    assert_eq!(state.fuels, [Some(100000)]);
}

#[test]
fn explicit_catalog_conflict_refuses_before_registry_or_initialization() {
    let selected =
        services(Arc::new(Mutex::new(Observations::default()))).with_capture_certification();
    let other = Arc::new(HostApiBuilder::new().build().unwrap());
    let result =
        selected.compile_options(CompileSourceFileOptions::new().with_host_api_catalog(other));
    let error = match result {
        Ok(_) => panic!("conflicting compiler catalog admitted"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("conflicts with host services"));
}

#[test]
#[ignore = "exact child-process REPL probe"]
fn opted_in_repl_child_probe() {
    if std::env::var_os("PD_NEUTRAL_CERT_REPL_CHILD").is_none() {
        return;
    }
    let observations = Arc::new(Mutex::new(Observations::default()));
    run_with_args(
        &CliRuntime::default(),
        &["--repl".into()],
        &services(Arc::clone(&observations)).with_capture_certification(),
    )
    .unwrap();
    let state = observations.lock().unwrap();
    assert!(!state.init_certified.is_empty());
    assert!(state.init_certified.iter().all(|valid| !valid));
    assert_eq!((state.calls, state.live, state.unavailable), (1, 0, 1));
}

#[test]
fn scripted_opted_in_repl_stays_uncertified_with_real_resource_closure() {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    let source = "let owner = neutral::open(); let dispatch = || if true => { neutral::check(); 41 } else => { neutral::close(owner); 41 }; dispatch();\n";
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "cli::capture_certification_tests::opted_in_repl_child_probe",
            "--nocapture",
        ])
        .env("PD_NEUTRAL_CERT_REPL_CHILD", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(source.as_bytes())
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            let _ = child.wait();
            panic!("scripted REPL timed out");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let stdout = out.join().unwrap();
    let stderr = err.join().unwrap();
    assert!(
        status.success(),
        "REPL child failed: {}\n{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
    assert!(String::from_utf8_lossy(&stdout).contains("1 passed; 0 failed"));
}

#[test]
fn opted_in_epoch_configuration_and_interrupt_conflict_keep_legacy_rules() {
    let scratch = Scratch::new();
    let source = scratch.source();
    let observations = Arc::new(Mutex::new(Observations::default()));
    let selected = services(Arc::clone(&observations)).with_capture_certification();
    run_with_args(
        &CliRuntime::default(),
        &[
            "--epoch-deadline".into(),
            "32".into(),
            source.display().to_string(),
        ],
        &selected,
    )
    .unwrap();
    {
        let state = observations.lock().unwrap();
        assert_eq!(state.init_certified, [true]);
        assert_eq!(state.deadlines, [Some(32)]);
        assert_eq!(state.fuels, [None]);
        assert_eq!(state.live, 1);
    }
    let error = run_with_args(
        &CliRuntime::default(),
        &[
            "--fuel".into(),
            "100000".into(),
            "--epoch-deadline".into(),
            "32".into(),
            source.display().to_string(),
        ],
        &selected,
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "--fuel and --epoch-deadline are mutually exclusive"
    );
    assert_eq!(observations.lock().unwrap().calls, 1);
}

#[test]
fn malformed_vmbc_keeps_wire_error_type_in_all_ordinary_cli_routes() {
    let scratch = Scratch::new();
    let path = scratch.0.join("malformed.vmbc");
    std::fs::write(&path, b"invalid VMBC bytes").unwrap();
    let expected = crate::decode_program(&std::fs::read(&path).unwrap())
        .unwrap_err()
        .to_string();
    for selected in [
        CliHostServices::default(),
        CliHostServices::default().with_capture_certification(),
        services(Arc::new(Mutex::new(Observations::default()))),
        services(Arc::new(Mutex::new(Observations::default()))).with_capture_certification(),
    ] {
        for emit in [false, true] {
            let mut args = Vec::new();
            if emit {
                args.extend([
                    "--emit-vmbc".to_owned(),
                    scratch.0.join("rejected.vmbc").display().to_string(),
                ]);
            }
            args.push(path.display().to_string());
            let error = run_with_args(&CliRuntime::default(), &args, &selected).unwrap_err();
            assert_eq!(error.to_string(), expected);
            assert!(
                error.downcast_ref::<crate::WireError>().is_some(),
                "WireError was wrapped: {error:?}"
            );
            assert!(error.downcast_ref::<io::Error>().is_none());
            assert!(!scratch.0.join("rejected.vmbc").exists());
        }
    }
}

#[test]
fn default_services_remain_uncertified_with_actual_owner_candidate() {
    let scratch = Scratch::new();
    let observations = Arc::new(Mutex::new(Observations::default()));
    run_with_args(
        &CliRuntime::default(),
        &[scratch.source().display().to_string()],
        &services(Arc::clone(&observations)),
    )
    .unwrap();
    let state = observations.lock().unwrap();
    assert_eq!(state.init_certified, [false]);
    assert_eq!((state.calls, state.live, state.unavailable), (1, 0, 1));
}

#[test]
fn opted_in_vmbc_retains_ordinary_bytes_and_no_capture_authority() {
    let scratch = Scratch::new();
    let source = scratch.source();
    let artifact = scratch.0.join("owner.vmbc");
    let observations = Arc::new(Mutex::new(Observations::default()));
    let selected = services(Arc::clone(&observations)).with_capture_certification();
    run_with_args(
        &CliRuntime::default(),
        &[
            "--emit-vmbc".into(),
            artifact.display().to_string(),
            source.display().to_string(),
        ],
        &selected,
    )
    .unwrap();
    let ordinary = compile_source_file_with_options(
        &source,
        CompileSourceFileOptions::new().with_host_api_catalog(catalog()),
    )
    .unwrap();
    assert_eq!(
        std::fs::read(&artifact).unwrap(),
        encode_program(&ordinary.program).unwrap()
    );
    run_with_args(
        &CliRuntime::default(),
        &[artifact.display().to_string()],
        &selected,
    )
    .unwrap();
    let state = observations.lock().unwrap();
    assert_eq!(state.init_certified, [true, false]);
    assert_eq!((state.calls, state.live, state.unavailable), (1, 0, 1));
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn source_companion_and_source_deleted_aot_never_reissue_capture_facts() {
    let scratch = Scratch::new();
    let source = scratch.source();
    let artifact = scratch.0.join("owner.pat");
    let observations = Arc::new(Mutex::new(Observations::default()));
    let selected = services(Arc::clone(&observations)).with_capture_certification();
    run_with_args(
        &CliRuntime::default(),
        &[
            "--aot-save".into(),
            artifact.display().to_string(),
            source.display().to_string(),
        ],
        &selected,
    )
    .unwrap();
    run_with_args(
        &CliRuntime::default(),
        &[
            "--aot-load".into(),
            artifact.display().to_string(),
            source.display().to_string(),
        ],
        &selected,
    )
    .unwrap();
    std::fs::remove_file(source).unwrap();
    run_with_args(
        &CliRuntime::default(),
        &["--aot-load".into(), artifact.display().to_string()],
        &selected,
    )
    .unwrap();
    let state = observations.lock().unwrap();
    assert_eq!(state.init_certified, [true, true, false]);
    assert_eq!(state.query_certified, [true, false, false]);
    eprintln!(
        "AOT owner bounds: init={:?}, query={:?}, calls/live/unavailable={:?}",
        state.init_certified,
        state.query_certified,
        (state.calls, state.live, state.unavailable)
    );
    assert_eq!((state.calls, state.live, state.unavailable), (3, 0, 3));
}

#[test]
fn failed_initialization_drops_resources_before_any_guest_effect() {
    struct Cleanup(Arc<AtomicUsize>);
    impl HostResource for Cleanup {}
    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let scratch = Scratch::new();
    let source = scratch.source();
    let observations = Arc::new(Mutex::new(Observations::default()));
    let dropped = Arc::new(AtomicUsize::new(0));
    let observed_drop = Arc::clone(&dropped);
    let selected = services(Arc::clone(&observations))
        .with_capture_certification()
        .with_vm_init(move |vm| {
            assert!(vm.capture_certification_stats().unwrap().valid);
            vm.host_context()
                .push_resource(Cleanup(Arc::clone(&observed_drop)))
                .unwrap();
            Err(VmError::HostError("neutral initialization refusal".into()))
        });
    let error = run_with_args(
        &CliRuntime::default(),
        &[source.display().to_string()],
        &selected,
    )
    .unwrap_err();
    assert!(error.to_string().contains("neutral initialization refusal"));
    assert_eq!(observations.lock().unwrap().calls, 0);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn certified_recording_keeps_owner_and_legacy_emit_requires_no_binding() {
    let scratch = Scratch::new();
    let source = scratch.source();
    let recording = scratch.0.join("owner.recording");
    let observations = Arc::new(Mutex::new(Observations::default()));
    run_with_args(
        &CliRuntime::default(),
        &[
            "--record".into(),
            recording.display().to_string(),
            source.display().to_string(),
        ],
        &services(Arc::clone(&observations)).with_capture_certification(),
    )
    .unwrap();
    assert!(VmRecording::load_from_file(&recording).is_ok());
    assert_eq!(observations.lock().unwrap().live, 1);
    fn compile_catalog() -> CompileSourceFileOptions {
        CompileSourceFileOptions::new().with_host_api_catalog(catalog())
    }
    let runtime = CliRuntime {
        compile_options: compile_catalog,
        ..CliRuntime::default()
    };
    let output = scratch.0.join("unbound.vmbc");
    run_with_args(
        &runtime,
        &[
            "--emit-vmbc".into(),
            output.display().to_string(),
            source.display().to_string(),
        ],
        &CliHostServices::default().with_capture_certification(),
    )
    .unwrap();
    assert!(crate::decode_program(&std::fs::read(output).unwrap()).is_ok());
}

#[test]
fn certified_source_cli_preserves_issued_owner_and_configuration() {
    let scratch = Scratch::new();
    let source = scratch.source();
    let observations = Arc::new(Mutex::new(Observations::default()));
    let services = services(Arc::clone(&observations)).with_capture_certification();
    run_with_args(
        &CliRuntime::default(),
        &[
            "--fuel".into(),
            "100000".into(),
            "--jit-hot-loop".into(),
            "7".into(),
            "--max-call-depth".into(),
            "32".into(),
            source.display().to_string(),
        ],
        &services,
    )
    .unwrap();
    let state = observations.lock().unwrap();
    assert_eq!(
        state.init_certified,
        [true],
        "source facts must precede vm_init"
    );
    assert_eq!(state.thresholds, [7]);
    assert_eq!(state.depths, [32]);
    assert_eq!(state.deadlines, [None]);
    assert_eq!(state.fuels, [Some(100000)]);
    assert_eq!((state.calls, state.live, state.unavailable), (1, 1, 0));
}
