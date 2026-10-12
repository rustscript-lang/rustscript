#![cfg(feature = "cli")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use vm::cli::{CliHostServices, CliRuntime, run_with_args};
use vm::{
    CallOutcome, CallReturn, CompileSourceFileOptions, HostApiBuilder, HostFunctionRegistry,
    HostFunctionSchema, HostParamSchema, HostTypeSchema, Value, VmError,
};

struct Scale(Arc<Mutex<Vec<i64>>>);
impl vm::HostFunction for Scale {
    fn call(&mut self, vm: &mut vm::Vm, args: &[Value]) -> vm::VmResult<CallOutcome> {
        let factor = *vm.host_context().module_state::<i64>().unwrap();
        let Value::Int(value) = args[0] else {
            return Err(VmError::TypeMismatch("int"));
        };
        self.0.lock().unwrap().push(value * factor);
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(
            value * factor,
        ))))
    }
}

fn scalar_services(
    seen: Arc<Mutex<Vec<i64>>>,
    inits: Arc<AtomicUsize>,
) -> CliHostServices<'static> {
    let mut builder = HostApiBuilder::new();
    builder.function(HostFunctionSchema::with_return(
        "measure::scale",
        vec![HostParamSchema::value("value", HostTypeSchema::Int)],
        HostTypeSchema::Int,
    ));
    let catalog = Arc::new(builder.build().unwrap());
    let registration_catalog = Arc::clone(&catalog);
    CliHostServices::new(catalog, move || {
        let mut registry = HostFunctionRegistry::new();
        let output = Arc::clone(&seen);
        registry
            .register_catalog(
                vm::catalog_import_schemas(&registration_catalog, "measure::scale").remove(0),
                move || Box::new(Scale(Arc::clone(&output))),
            )
            .map_err(|err| VmError::HostError(err.to_string()))?;
        Ok(registry)
    })
    .with_vm_init(move |vm| {
        inits.fetch_add(1, Ordering::SeqCst);
        vm.host_context().set_module_state(3_i64);
        Ok(())
    })
}

fn runtime() -> CliRuntime {
    // External caller of the original three-field API and process entrypoint.
    let _: fn(CliRuntime) -> Result<(), Box<dyn std::error::Error>> = vm::cli::main;
    CliRuntime {
        binary_name: "embedding-probe",
        default_source: "unused.rss",
        compile_options: CompileSourceFileOptions::default,
    }
}

struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new(name: &str, source: &str) -> Self {
        let path = std::env::temp_dir().join(format!("cli-{}-{}.rss", name, std::process::id()));
        std::fs::write(&path, source).unwrap();
        Self(path)
    }
    fn arg(&self) -> String {
        self.0.display().to_string()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn public_cli_executes_typed_scalar_with_per_vm_initialization() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let services = scalar_services(Arc::clone(&seen), Arc::new(AtomicUsize::new(0)));
    let source = Fixture::new("scalar", "measure::scale(14);");
    let compiled = vm::compile_source_for_repl_with_state_and_options(
        "measure::scale(14);",
        &[],
        services
            .compile_options(CompileSourceFileOptions::default())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(compiled.compiled.program.imports[0].name, "measure::scale");
    run_with_args(&runtime(), &[source.arg()], &services).unwrap();
    assert_eq!(*seen.lock().unwrap(), vec![42]);
}

#[test]
fn public_cli_injects_hosts_into_vmbc_record_debug_and_native_paths() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let inits = Arc::new(AtomicUsize::new(0));
    let services = scalar_services(Arc::clone(&seen), Arc::clone(&inits));
    let source = Fixture::new("paths", "measure::scale(14);");
    let wire = source.0.with_extension("vmbc");
    let recording = source.0.with_extension("pdr");
    let aot = source.0.with_extension("pat");
    run_with_args(
        &runtime(),
        &[
            "--emit-vmbc".into(),
            wire.display().to_string(),
            source.arg(),
        ],
        &services,
    )
    .unwrap();
    run_with_args(&runtime(), &[wire.display().to_string()], &services).unwrap();
    run_with_args(
        &runtime(),
        &[
            "--record".into(),
            recording.display().to_string(),
            source.arg(),
        ],
        &services,
    )
    .unwrap();
    let recorded = vm::VmRecording::load_from_file(&recording).unwrap();
    assert!(!recorded.frames.is_empty());
    run_with_args(
        &runtime(),
        &["--debug".into(), "--no-stop-on-entry".into(), source.arg()],
        &services,
    )
    .unwrap();
    #[cfg(feature = "cranelift-jit")]
    {
        run_with_args(
            &runtime(),
            &["--jit-hot-loop".into(), "1".into(), source.arg()],
            &services,
        )
        .unwrap();
        run_with_args(
            &runtime(),
            &["--aot-save".into(), aot.display().to_string(), source.arg()],
            &services,
        )
        .unwrap();
        run_with_args(
            &runtime(),
            &["--aot-load".into(), aot.display().to_string()],
            &services,
        )
        .unwrap();
        run_with_args(
            &runtime(),
            &["--aot-load".into(), aot.display().to_string(), source.arg()],
            &services,
        )
        .unwrap();
    }
    let expected = if cfg!(feature = "cranelift-jit") {
        7
    } else {
        3
    };
    assert_eq!(*seen.lock().unwrap(), vec![42; expected]);
    assert_eq!(
        inits.load(Ordering::SeqCst),
        expected + 1,
        "emit validates and initializes its VM without guest execution"
    );
    for path in [wire, recording, aot] {
        let _ = std::fs::remove_file(path);
    }
}

#[test]
fn reloaded_artifact_cannot_use_a_registry_from_another_services_catalog() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let original = scalar_services(Arc::clone(&seen), Arc::new(AtomicUsize::new(0)));
    let options = original
        .compile_options(CompileSourceFileOptions::default())
        .unwrap();
    let schema =
        vm::catalog_import_schemas(options.host_api_catalog().unwrap(), "measure::scale").remove(0);
    let compiled = vm::compile_source_with_flavor_and_options(
        "measure::scale(14);",
        vm::SourceFlavor::RustScript,
        options,
    )
    .unwrap();
    let wire = Fixture::new("mismatch", "");
    let path = wire.0.with_extension("vmbc");
    std::fs::write(&path, vm::encode_program(&compiled.program).unwrap()).unwrap();
    let empty = Arc::new(HostApiBuilder::new().build().unwrap());
    let output = Arc::clone(&seen);
    let wrong = CliHostServices::new(empty, move || {
        let mut registry = HostFunctionRegistry::new();
        let output = Arc::clone(&output);
        registry
            .register_catalog(schema.clone(), move || Box::new(Scale(Arc::clone(&output))))
            .map_err(|e| VmError::HostError(e.to_string()))?;
        Ok(registry)
    })
    .with_vm_init(|vm| {
        vm.host_context().set_module_state(3_i64);
        Ok(())
    });
    let result = run_with_args(&runtime(), &[path.display().to_string()], &wrong);
    std::fs::remove_file(path).unwrap();
    assert!(
        result.is_err(),
        "a matching registry cannot substitute another services catalog"
    );
    assert!(
        seen.lock().unwrap().is_empty(),
        "rejection must precede guest effects"
    );
}

#[test]
fn legacy_three_field_cli_can_emit_an_artifact_for_an_unbound_embedding() {
    let runtime = CliRuntime {
        compile_options: || {
            scalar_services(
                Arc::new(Mutex::new(Vec::new())),
                Arc::new(AtomicUsize::new(0)),
            )
            .compile_options(CompileSourceFileOptions::default())
            .unwrap()
        },
        ..runtime()
    };
    let source = Fixture::new("legacy-emit", "measure::scale(14);");
    let wire = source.0.with_extension("vmbc");
    let result = run_with_args(
        &runtime,
        &[
            "--emit-vmbc".into(),
            wire.display().to_string(),
            source.arg(),
        ],
        &CliHostServices::default(),
    );
    result.unwrap();
    let decoded = vm::decode_program(&std::fs::read(&wire).unwrap()).unwrap();
    assert_eq!(decoded.imports[0].name, "measure::scale");
    std::fs::remove_file(wire).unwrap();
}

#[test]
fn repl_rejects_unsupported_module_options_instead_of_discarding_them() {
    let options = CompileSourceFileOptions::default()
        .with_module_override_source("calibration", "pub fn value() -> int { 42 }");
    let result = vm::compile_source_for_repl_with_state_and_options("1;", &[], options);
    assert!(
        result.is_err(),
        "native-only REPL must not silently discard module options"
    );
}

#[test]
fn services_preserve_catalog_conflicts_missing_bindings_and_restricted_capabilities() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let inits = Arc::new(AtomicUsize::new(0));
    let original = scalar_services(Arc::clone(&seen), Arc::clone(&inits));
    let options = original
        .compile_options(CompileSourceFileOptions::default())
        .unwrap();
    let catalog = options.host_api_catalog().unwrap().clone();
    let source = Fixture::new("denial", "measure::scale(14);");
    let absent = CliHostServices::new(Arc::clone(&catalog), || Ok(HostFunctionRegistry::new()));
    assert!(
        run_with_args(&runtime(), &[source.arg()], &absent)
            .unwrap_err()
            .to_string()
            .contains("measure::scale")
    );
    let schema = vm::catalog_import_schemas(&catalog, "measure::scale").remove(0);
    let denied = CliHostServices::new(catalog, move || {
        let mut registry = HostFunctionRegistry::restricted();
        registry
            .register_catalog(schema.clone(), || {
                Box::new(Scale(Arc::new(Mutex::new(Vec::new()))))
            })
            .map_err(|e| VmError::HostError(e.to_string()))?;
        Ok(registry)
    });
    assert!(
        run_with_args(&runtime(), &[source.arg()], &denied)
            .unwrap_err()
            .to_string()
            .contains("capability")
    );
    let conflicting = CompileSourceFileOptions::default()
        .with_host_api_catalog(Arc::new(HostApiBuilder::new().build().unwrap()));
    assert!(original.compile_options(conflicting).is_err());
    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(inits.load(Ordering::SeqCst), 0);
    // The existing unbound VM fails even with correctly compiled typed imports.
    let compiled = vm::compile_source_with_flavor_and_options(
        "measure::scale(14);",
        vm::SourceFlavor::RustScript,
        options,
    )
    .unwrap();
    let mut unbound = vm::Vm::new(compiled.program);
    assert!(unbound.run().is_err());
}

struct CalibrationFrontend;
static CALIBRATION_FRONTEND: CalibrationFrontend = CalibrationFrontend;
impl vm::SourcePlugin for CalibrationFrontend {
    fn descriptor(&self) -> vm::SourceFrontendDescriptor {
        vm::SourceFrontendDescriptor {
            id: vm::FrontendId::new("calibration-probe"),
            extensions: &["calib"],
            typing_policy: vm::FrontendTypingPolicy::Strict,
            supports_ownership_checking: true,
        }
    }
    fn parse_source(
        &self,
        source: &str,
        context: &vm::SourceFrontendContext<'_>,
    ) -> Result<vm::FrontendIr, vm::ParseError> {
        struct Grammar;
        impl vm::ParserDialect for Grammar {}
        static GRAMMAR: Grammar = Grammar;
        let mut ir = vm::parse_source_with_dialect(
            "",
            &GRAMMAR,
            vm::SharedParserOptions {
                source_id: context.source_id,
                allow_implicit_externs: false,
                allow_implicit_semicolons: false,
                enforce_mutable_bindings: true,
                import_scan_mode: false,
            },
        )?;
        let value = source
            .trim()
            .parse::<i64>()
            .map_err(|e| vm::ParseError::new(e.to_string()))?;
        let index = context.install_host_function(&mut ir, "measure::scale", 1)?;
        ir.stmts.push(vm::Stmt::Expr {
            expr: vm::Expr::Call(index, Vec::new(), vec![vm::Expr::Int(value)], None, None),
            line: 1,
        });
        Ok(ir)
    }
    fn parse_module_imports(
        &self,
        _: &str,
        _: &std::path::Path,
        _: &vm::SourceFrontendContext<'_>,
    ) -> Result<Vec<vm::ModuleImport>, vm::SourcePathError> {
        Ok(Vec::new())
    }
    fn validate_typed_ir(&self, typed: &vm::FrontendTypedIr<'_>) -> Result<(), vm::CompileError> {
        assert!(
            typed.host_api_catalog.is_some(),
            "effective catalog must reach the compiler hook"
        );
        if typed.ir.stmts.iter().any(|stmt| matches!(stmt, vm::Stmt::Expr { expr: vm::Expr::Call(_, _, args, ..), .. } if matches!(args.first(), Some(vm::Expr::Int(-1))))) {
            return Err(vm::CompileError::HostCallResolve { line: None, source_name: None, span: None, detail: "calibration hook rejected input".into() });
        }
        Ok(())
    }
}

#[test]
fn public_cli_preserves_language_neutral_frontend_options_and_typed_hooks() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let inits = Arc::new(AtomicUsize::new(0));
    let services = scalar_services(Arc::clone(&seen), Arc::clone(&inits));
    let runtime = CliRuntime {
        compile_options: || {
            CompileSourceFileOptions::default().with_source_plugin(&CALIBRATION_FRONTEND)
        },
        ..runtime()
    };
    let mut source = Fixture::new("plugin", "14");
    let extension = source.0.with_extension("calib");
    std::fs::rename(&source.0, &extension).unwrap();
    source.0 = extension;
    run_with_args(&runtime, &[source.arg()], &services).unwrap();
    std::fs::write(&source.0, "-1").unwrap();
    assert!(
        run_with_args(&runtime, &[source.arg()], &services)
            .unwrap_err()
            .to_string()
            .contains("calibration hook rejected input")
    );
    assert_eq!(*seen.lock().unwrap(), vec![42]);
    assert_eq!(inits.load(Ordering::SeqCst), 1);
    assert!(
        vm::compile_source_for_repl_with_state_and_options("1;", &[], (runtime.compile_options)())
            .is_err(),
        "native-only REPL must reject unsupported frontend hooks"
    );
}

// Subprocess harness only; never counted as a business test.
#[test]
#[ignore]
fn embedding_repl_child() {
    let inits = Arc::new(AtomicUsize::new(0));
    let services = scalar_services(Arc::new(Mutex::new(Vec::new())), Arc::clone(&inits));
    let args = std::env::var("EMBEDDING_CLI_ARGS")
        .map(|raw| serde_json::from_str::<Vec<String>>(&raw).unwrap())
        .unwrap_or_else(|_| vec!["repl".into()]);
    run_with_args(&runtime(), &args, &services).unwrap();
    println!("embedding-init-count={}", inits.load(Ordering::SeqCst));
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn standalone_aot_initializer_observes_the_same_runtime_flags_as_source() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let services =
        scalar_services(Arc::clone(&seen), Arc::new(AtomicUsize::new(0))).with_vm_init(|vm| {
            if vm.get_fuel() != Some(99) {
                return Err(VmError::HostError(
                    "initializer did not receive runtime flags".into(),
                ));
            }
            vm.host_context().set_module_state(3_i64);
            Ok(())
        });
    let source = Fixture::new("aot-flags", "measure::scale(14);");
    let aot = source.0.with_extension("pat");
    run_with_args(
        &runtime(),
        &[
            "--fuel".into(),
            "99".into(),
            "--aot-save".into(),
            aot.display().to_string(),
            source.arg(),
        ],
        &services,
    )
    .unwrap();
    let result = run_with_args(
        &runtime(),
        &[
            "--fuel".into(),
            "99".into(),
            "--aot-load".into(),
            aot.display().to_string(),
        ],
        &services,
    );
    std::fs::remove_file(aot).unwrap();
    result.unwrap();
    assert_eq!(*seen.lock().unwrap(), vec![42, 42]);
}

#[cfg(feature = "cranelift-jit")]
#[test]
fn public_cli_runs_native_jit_work_in_the_initialized_vm() {
    let source = Fixture::new(
        "native-jit",
        "let mut i = 0; let mut total = 0; while i < 1000 { total = total + 1; i = i + 1; } measure::scale(total);",
    );
    let args = vec![
        "--jit-hot-loop".to_string(),
        "1".to_string(),
        "--jit-dump".to_string(),
        "--jit-dump-no-code".to_string(),
        source.arg(),
    ];
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "embedding_repl_child",
            "--nocapture",
        ])
        .env("EMBEDDING_CLI_ARGS", serde_json::to_string(&args).unwrap())
        .output()
        .unwrap();
    let out = String::from_utf8(result.stdout).unwrap();
    assert!(
        result.status.success(),
        "{out}\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(out.contains("stack: [Int(3000)]"), "{out}");
    let compiled = out
        .lines()
        .find_map(|line| line.trim().strip_prefix("native traces: "))
        .unwrap()
        .parse::<usize>()
        .unwrap();
    assert!(compiled > 0, "{out}");
    assert!(out.contains("embedding-init-count=1"), "{out}");
}

#[test]
fn public_cli_repl_uses_catalog_and_initializer_across_snippets() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "embedding_repl_child",
            "--nocapture",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"let x = measure::scale(14);\nx + 1\nmeasure::scale(x)\n.quit\n")
        .unwrap();
    let result = child.wait_with_output().unwrap();
    let out = String::from_utf8(result.stdout).unwrap();
    assert!(
        result.status.success(),
        "{out}\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(out.contains("=> 43"), "{out}");
    assert!(out.contains("=> 126"), "{out}");
    assert!(out.contains("embedding-init-count=3"), "{out}");
}
