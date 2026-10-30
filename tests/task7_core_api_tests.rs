//! xeHentai Task7's two Core standard-library prerequisites through real RSS imports.

#![cfg(feature = "runtime")]

#[cfg(feature = "async")]
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use vm::compiler::{
    CompileSourceFileOptions, SourceFlavor, compile_source_with_flavor_and_options,
};
use vm::host_api::{HostApiCatalog, HostParamPassing};
use vm::{
    CompiledProgram, HostFunctionRegistry, HostStructField, HostTypeSchema, IoHostExt, IoPolicy,
    SourcePathError, Value, Vm, VmError, VmStatus, standard_host_catalog, standard_host_modules,
};
#[cfg(feature = "async")]
use vm::{HostFuture, HostFutureOutput, HostOpId, VmResult};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

type SubmittedOps = Arc<AtomicUsize>;

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(label: &str) -> Self {
        let lease_tmp = PathBuf::from(
            std::env::var_os("XEH_LEASE_TMP")
                .expect("XEH_LEASE_TMP must name the allocated task temp"),
        );
        let parent = lease_tmp.join("task7-core-api-tests");
        fs::create_dir_all(&parent).expect("task-owned temp parent should be creatable");
        let ordinal = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should follow the Unix epoch")
            .as_nanos();
        let dir = parent.join(format!("{label}-{}-{ordinal}-{nonce}", std::process::id()));
        fs::create_dir(&dir).expect("task-owned scratch directory should be creatable");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn compile(source: &str, catalog: Arc<HostApiCatalog>) -> Result<CompiledProgram, SourcePathError> {
    compile_source_with_flavor_and_options(
        source,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(catalog),
    )
}

fn install_catalog_module(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
    name: &str,
) {
    let module = standard_host_modules()
        .iter()
        .find(|module| module.name == name)
        .unwrap_or_else(|| panic!("standard {name} module must exist"));
    module
        .catalog_module()
        .unwrap_or_else(|| panic!("standard {name} module must publish a catalog"))
        .install_from_catalog(registry, catalog)
        .unwrap_or_else(|error| panic!("standard {name} module must install: {error}"));
}

fn run_vm_with_catalog(
    source: &str,
    catalog: Arc<HostApiCatalog>,
    policy: Option<IoPolicy>,
) -> (Vm, SubmittedOps) {
    let compiled = compile(source, Arc::clone(&catalog)).expect("RSS source should compile");
    let mut vm = Vm::try_new(compiled.program).expect("test VM should construct");
    let mut registry = HostFunctionRegistry::empty();
    install_catalog_module(&mut registry, catalog.as_ref(), "json");
    install_catalog_module(&mut registry, catalog.as_ref(), "io");
    registry
        .bind_vm_cached(&mut vm)
        .expect("the exact catalog modules should bind");
    if let Some(policy) = policy {
        vm.configure_io(policy);
    }
    let submitted = install_async_driver(&mut vm);
    (vm, submitted)
}

fn run_standard(source: &str, policy: Option<IoPolicy>) -> (Vm, SubmittedOps) {
    run_vm_with_catalog(source, standard_host_catalog(), policy)
}

fn run_to_halt(vm: &mut Vm) -> Result<usize, VmError> {
    let mut pending_operations = 0usize;
    let mut status = vm.run()?;
    loop {
        match status {
            VmStatus::Halted => return Ok(pending_operations),
            VmStatus::Yielded => status = vm.resume()?,
            VmStatus::Waiting(_) => {
                vm.wait_for_host_op_blocking()?;
                pending_operations += 1;
                status = vm.resume()?;
            }
        }
    }
}

fn assert_backend_driver(submitted: &SubmittedOps, pending_operations: usize) {
    let submitted = submitted.load(Ordering::SeqCst);
    assert!(
        pending_operations > 0,
        "IO must use the selected pending-operation driver"
    );
    #[cfg(feature = "async")]
    assert_eq!(
        submitted, pending_operations,
        "the async bridge must resolve every pending call"
    );
    #[cfg(not(feature = "async"))]
    assert_eq!(
        submitted, 0,
        "the worker backend must not submit async futures"
    );
}

#[cfg(feature = "async")]
fn install_async_driver(vm: &mut Vm) -> SubmittedOps {
    install_async_driver_with_cancel_observer(vm, None)
}

#[cfg(feature = "async")]
fn install_async_driver_with_cancel_tracking(
    vm: &mut Vm,
    cancelled: Arc<AtomicUsize>,
) -> SubmittedOps {
    install_async_driver_with_cancel_observer(vm, Some(cancelled))
}

#[cfg(feature = "async")]
fn install_async_driver_with_cancel_observer(
    vm: &mut Vm,
    cancelled: Option<Arc<AtomicUsize>>,
) -> SubmittedOps {
    use std::task::{Context, Poll};

    use vm::{CallReturn, HostAsyncBridge};

    struct TokioHostDriver {
        runtime: tokio::runtime::Runtime,
        submitted: HashMap<HostOpId, HostFuture>,
        count: SubmittedOps,
        cancelled: Option<Arc<AtomicUsize>>,
    }

    impl TokioHostDriver {
        fn new(count: SubmittedOps, cancelled: Option<Arc<AtomicUsize>>) -> Self {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("test host runtime should build");
            Self {
                runtime,
                submitted: HashMap::new(),
                count,
                cancelled,
            }
        }
    }

    impl HostAsyncBridge for TokioHostDriver {
        fn submit_op(&mut self, op_id: HostOpId, future: HostFuture) -> VmResult<()> {
            if self.submitted.insert(op_id, future).is_some() {
                return Err(VmError::HostError(format!(
                    "duplicate submitted host op {op_id}"
                )));
            }
            self.count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn poll_op(
            &mut self,
            op_id: HostOpId,
            _cx: &mut Context<'_>,
        ) -> Poll<VmResult<CallReturn>> {
            Poll::Ready(Err(VmError::HostError(format!(
                "unknown external host operation {op_id}"
            ))))
        }

        fn poll_submitted_op(
            &mut self,
            op_id: HostOpId,
            cx: &mut Context<'_>,
        ) -> Poll<VmResult<HostFutureOutput>> {
            let poll = {
                let Some(future) = self.submitted.get_mut(&op_id) else {
                    return Poll::Ready(Err(VmError::HostError(format!(
                        "unknown submitted host operation {op_id}"
                    ))));
                };
                let _guard = self.runtime.enter();
                future.as_mut().poll(cx)
            };
            if poll.is_ready() {
                self.submitted.remove(&op_id);
            }
            poll
        }

        fn cancel_op(&mut self, op_id: HostOpId) {
            self.submitted.remove(&op_id);
        }

        fn request_cancel_op(
            &mut self,
            op_id: HostOpId,
            _reason: vm::operation::OperationCancelReason,
        ) -> VmResult<()> {
            if self.submitted.remove(&op_id).is_some()
                && let Some(cancelled) = self.cancelled.as_ref()
            {
                cancelled.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }

        fn poll_cancel_op(
            &mut self,
            _op_id: HostOpId,
            _cx: &mut Context<'_>,
        ) -> Poll<VmResult<()>> {
            Poll::Ready(Ok(()))
        }
    }

    let count = SubmittedOps::default();
    vm.set_async_bridge(Box::new(TokioHostDriver::new(
        Arc::clone(&count),
        cancelled,
    )))
    .expect("test host async driver should install");
    count
}

#[cfg(not(feature = "async"))]
fn install_async_driver(_vm: &mut Vm) -> SubmittedOps {
    SubmittedOps::default()
}

fn host_error(error: VmError) -> String {
    match error {
        VmError::HostError(message) => message,
        other => format!("{other:?}"),
    }
}

#[test]
fn task7_legacy_decode_failure_stops_guest_execution() {
    let source = r#"
use json;
pub fn legacy_decode() {
    let decoded: array = json::decode::<array>("[");
    assert(false);
    9;
}
legacy_decode();
"#;
    let (mut vm, _) = run_standard(source, None);
    let error =
        run_to_halt(&mut vm).expect_err("invalid JSON must fail before recovery API exists");
    let message = host_error(error);
    println!("legacy-json-invalid-stops-guest: {message}");
    assert!(
        message.contains("json_decode failed"),
        "unexpected error: {message}"
    );
    assert!(
        !message.contains("assertion failed"),
        "guest continued after invalid JSON"
    );
}

#[test]
fn task7_legacy_write_limit_failure_stops_guest_execution() {
    let scratch = ScratchDir::new("legacy-write");
    let path = scratch.path().join("legacy.txt");
    fs::write(&path, b"prior-bytes").expect("sentinel should be written");
    let source = format!(
        r#"
use io;
pub fn legacy_write() {{
    let file = io::open({path}, "w");
    io::write(file, "too-long");
    assert(false);
    9;
}}
legacy_write();
"#,
        path = serde_json::to_string(path.to_str().expect("UTF-8 fixture path")).unwrap(),
    );
    let policy = IoPolicy {
        allowed_roots: vec![scratch.path().to_string_lossy().into_owned()],
        allow_write: true,
        allow_process: false,
        max_read_bytes: 1024,
        max_write_bytes: 1,
    };
    let (mut vm, submitted) = run_standard(&source, Some(policy));
    let error = run_to_halt(&mut vm).expect_err("legacy write limit must abort the guest call");
    let message = host_error(error);
    println!(
        "legacy-write-limit-stops-guest: {message}; bytes={:?}",
        fs::read(&path)
    );
    assert!(
        message.contains("io_write exceeded write limit"),
        "unexpected error: {message}"
    );
    assert!(
        !message.contains("assertion failed"),
        "guest continued after write failure"
    );
    #[cfg(feature = "async")]
    {
        assert!(submitted.load(Ordering::SeqCst) >= 1);
    }
    #[cfg(not(feature = "async"))]
    assert_backend_driver(&submitted, 1);
    assert!(vm.execution_scope().operations().is_empty());
}

#[test]
fn task7_json_is_valid_schema_and_guest_continuation() {
    let source = r#"
use json;
pub fn validate_and_continue() {
    assert(json::is_valid("null"));
    assert(json::is_valid("false"));
    assert(json::is_valid("0"));
    assert(json::is_valid("\"text\""));
    assert(json::is_valid("[]"));
    assert(json::is_valid("{}"));
    assert(!json::is_valid("["));
    assert(!json::is_valid("{\"outer\":{\"key\":1,\"key\":2}}"));
    assert(!json::is_valid("null true"));
    assert(!json::is_valid("NaN"));
    assert(!json::is_valid("\"\\uD800\""));
    let deep = json::is_valid("DEEP_JSON");
    assert(deep == DEEP_EXPECTED);
    let malformed = "[";
    if json::is_valid(malformed.copy()) {
        let impossible: array = json::decode::<array>(malformed.copy());
        assert(impossible.length == 0);
    }
    let decoded: array = json::decode::<array>("[1,2]");
    assert(decoded.length == 2);
    73;
}
validate_and_continue();
"#;
    let deep_json = format!("{}null{}", "[".repeat(160), "]".repeat(160));
    let deep_expected = serde_json::from_str::<serde_json::Value>(&deep_json).is_ok();
    let source = source.replace("DEEP_JSON", &deep_json).replace(
        "DEEP_EXPECTED",
        if deep_expected { "true" } else { "false" },
    );
    let (mut vm, _) = run_standard(&source, None);
    run_to_halt(&mut vm).expect("invalid JSON must be inspectable and healthy decode must run");
    assert_eq!(vm.stack().last(), Some(&Value::Int(73)));
    assert_eq!(vm.host_context().resource_count(), 0);
    assert!(vm.execution_scope().operations().is_empty());

    let catalog = standard_host_catalog();
    let schemas = catalog.functions_named("json::is_valid");
    let schema = schemas
        .first()
        .expect("standard catalog must publish json::is_valid");
    assert_eq!(schema.params.len(), 1);
    assert_eq!(schema.params[0].name, "text");
    assert_eq!(schema.params[0].ty, HostTypeSchema::String);
    assert_eq!(schema.params[0].passing, HostParamPassing::Value);
    assert_eq!(schema.return_type, HostTypeSchema::Bool);
    assert_eq!(vm::builtin_call_index("json::is_valid"), None);
    assert_eq!(vm::builtin_call_index("json::decode"), Some(0xFFC4));
}

#[test]
fn task7_json_is_valid_uses_custom_catalog_and_unbound_import_fails_closed() {
    let module = standard_host_modules()
        .iter()
        .find(|module| module.name == "json")
        .expect("standard json module must exist");
    let custom_catalog = Arc::new(
        module
            .catalog_module()
            .expect("json module catalog must exist")
            .catalog()
            .expect("json module catalog must be valid"),
    );
    let source = "use json; pub fn custom_validate() { assert(!json::is_valid(\"[\")); 17; } custom_validate();";
    let compiled =
        compile(source, Arc::clone(&custom_catalog)).expect("custom source should compile");
    let mut vm = Vm::try_new(compiled.program).expect("test VM should construct");
    let mut registry = HostFunctionRegistry::empty();
    install_catalog_module(&mut registry, custom_catalog.as_ref(), "json");
    registry
        .bind_vm_cached(&mut vm)
        .expect("the custom json catalog should bind");
    let _submitted = install_async_driver(&mut vm);
    run_to_halt(&mut vm).expect("custom json catalog should bind and run");
    assert_eq!(vm.stack().last(), Some(&Value::Int(17)));

    let compiled =
        compile(source, standard_host_catalog()).expect("standard source should compile");
    let mut unbound_vm = Vm::try_new(compiled.program).expect("test VM should construct");
    let error = HostFunctionRegistry::restricted()
        .bind_vm_cached(&mut unbound_vm)
        .expect_err("unregistered json::is_valid must fail closed");
    assert!(
        error.to_string().contains("json::is_valid"),
        "unexpected unbound import error: {error}"
    );
}

#[test]
fn task7_io_result_schema_and_standard_module_composition() {
    let catalog = standard_host_catalog();
    let result = catalog
        .struct_named("IoWriteTextResult")
        .expect("standard catalog must publish IoWriteTextResult");
    let fields: Vec<(&str, &HostTypeSchema)> = result
        .fields
        .iter()
        .map(|field: &HostStructField| (field.name.as_str(), &field.ty))
        .collect();
    assert_eq!(
        fields,
        [
            ("ok", &HostTypeSchema::Bool),
            ("error_code", &HostTypeSchema::String)
        ]
    );
    let schemas = catalog.functions_named("io::try_write_text");
    let schema = schemas
        .first()
        .expect("standard catalog must publish io::try_write_text");
    assert_eq!(schema.params.len(), 2);
    assert_eq!(schema.params[0].name, "path");
    assert_eq!(schema.params[0].ty, HostTypeSchema::String);
    assert_eq!(schema.params[0].passing, HostParamPassing::Value);
    assert_eq!(schema.params[1].name, "text");
    assert_eq!(schema.params[1].ty, HostTypeSchema::String);
    assert_eq!(schema.params[1].passing, HostParamPassing::Value);
    assert_eq!(schema.return_type, result.as_type());
    assert_eq!(vm::builtin_call_index("io::try_write_text"), None);
    assert!(catalog.struct_named("JsonObjectEntry").is_some());
    assert!(catalog.struct_named("JitConfig").is_some());
    assert!(catalog.struct_named("SqliteQueryResult").is_some());
}

#[test]
fn task7_try_write_text_uses_custom_catalog_and_unbound_import_fails_closed() {
    let scratch = ScratchDir::new("custom-write");
    let target = scratch.path().join("custom.txt");
    let module = standard_host_modules()
        .iter()
        .find(|module| module.name == "io")
        .expect("standard io module must exist");
    let custom_catalog = Arc::new(
        module
            .catalog_module()
            .expect("io module catalog must exist")
            .catalog()
            .expect("io module catalog must be valid"),
    );
    let path = serde_json::to_string(target.to_str().expect("fixture path is UTF-8")).unwrap();
    let source = format!(
        "use io; pub fn custom_write() {{ let result = io::try_write_text({path}, \"custom\"); assert(result.ok); assert(result.error_code == \"\"); 47; }} custom_write();"
    );
    let compiled =
        compile(&source, Arc::clone(&custom_catalog)).expect("custom io source should compile");
    let mut vm = Vm::try_new(compiled.program).expect("test VM should construct");
    let mut registry = HostFunctionRegistry::empty();
    install_catalog_module(&mut registry, custom_catalog.as_ref(), "io");
    registry
        .bind_vm_cached(&mut vm)
        .expect("the custom io catalog should bind");
    vm.configure_io(IoPolicy {
        allowed_roots: vec![scratch.path().to_string_lossy().into_owned()],
        allow_write: true,
        allow_process: false,
        max_read_bytes: 64,
        max_write_bytes: 64,
    });
    let submitted = install_async_driver(&mut vm);
    let pending = run_to_halt(&mut vm).expect("custom catalog write should run");
    assert_backend_driver(&submitted, pending);
    assert_eq!(vm.stack().last(), Some(&Value::Int(47)));
    assert_eq!(
        fs::read(&target).expect("custom write should create output"),
        b"custom"
    );

    let standard_program = compile(&source, standard_host_catalog())
        .expect("standard import should compile before binding");
    let mut unbound_vm = Vm::try_new(standard_program.program).expect("test VM should construct");
    let error = HostFunctionRegistry::restricted()
        .bind_vm_cached(&mut unbound_vm)
        .expect_err("unregistered io::try_write_text must fail closed");
    assert!(
        error.to_string().contains("io::try_write_text"),
        "unexpected unbound import error: {error}"
    );
}

#[test]
fn task7_try_write_text_returns_typed_errors_and_continues_after_native_io() {
    let scratch = ScratchDir::new("try-write");
    let allowed = scratch.path().join("allowed");
    fs::create_dir(&allowed).expect("allowed root should be created");
    let exact_path = allowed.join("exact.txt");
    let over_path = allowed.join("over.txt");
    let directory_path = allowed.join("directory-target");
    let missing_parent_path = allowed.join("missing-parent").join("child.txt");
    let unrelated_path = allowed.join("unrelated.txt");
    let legacy_path = allowed.join("legacy-success.txt");
    let after_path = allowed.join("after.txt");
    let outside_path = scratch.path().join("outside.txt");
    fs::write(&exact_path, b"sentinel-exact").expect("existing exact target should be written");
    fs::write(&over_path, b"sentinel-over").expect("existing oversize target should be written");
    fs::create_dir(&directory_path).expect("directory failure target should be created");
    fs::write(&unrelated_path, b"unrelated-resource")
        .expect("unrelated resource fixture should be written");
    fs::write(&legacy_path, b"old").expect("legacy typed IO fixture should be written");

    let encoded = |path: &Path| {
        serde_json::to_string(path.to_str().expect("fixture paths are UTF-8")).unwrap()
    };
    let source = format!(
        r#"
use io;
pub fn task7_write() {{
    let unrelated = io::open({unrelated}, "r");
    let exact = io::try_write_text({exact}, "雪é");
    let too_large = io::try_write_text({over}, "雪éx");
    let denied = io::try_write_text({outside}, "outside");
    let directory_error = io::try_write_text({directory}, "x");
    let missing_error = io::try_write_text({missing}, "failure");
    let legacy = io::open({legacy}, "w");
    let written = io::write(legacy, "base");
    assert(written == 4);
    io::flush(legacy);
    assert(io::close(legacy));
    let contents = io::read_all(unrelated);
    assert(contents == "unrelated-resource");
    assert(io::close(unrelated));
    let after = io::try_write_text({after}, "after");
    [exact.ok, exact.error_code == "", too_large.ok, too_large.error_code == "write_limit", denied.ok, denied.error_code == "policy_denied", directory_error.ok, directory_error.error_code == "io_error", missing_error.ok, missing_error.error_code == "io_error", after.ok, after.error_code == ""];
}}
task7_write();
"#,
        unrelated = encoded(&unrelated_path),
        exact = encoded(&exact_path),
        over = encoded(&over_path),
        outside = encoded(&outside_path),
        directory = encoded(&directory_path),
        missing = encoded(&missing_parent_path),
        legacy = encoded(&legacy_path),
        after = encoded(&after_path),
    );
    let policy = IoPolicy {
        allowed_roots: vec![allowed.to_string_lossy().into_owned()],
        allow_write: true,
        allow_process: false,
        max_read_bytes: 1024,
        max_write_bytes: 5,
    };
    let (mut vm, submitted) = run_standard(&source, Some(policy));
    let pending = run_to_halt(&mut vm).expect("ordinary write failures must return typed results");
    assert_backend_driver(&submitted, pending);
    let Some(Value::Array(outcomes)) = vm.stack().last() else {
        panic!("guest must return the write outcome checks");
    };
    assert_eq!(
        outcomes.as_ref(),
        &vec![
            Value::Bool(true),
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(true),
            Value::Bool(true),
            Value::Bool(true),
        ],
        "ordinary write failures should preserve their stable result codes",
    );
    assert_eq!(
        fs::read(&exact_path).expect("exact output should be readable"),
        "雪é".as_bytes()
    );
    assert_eq!(
        fs::read(&over_path).expect("oversize target should remain readable"),
        b"sentinel-over"
    );
    assert!(
        !outside_path.exists(),
        "outside-root rejection must not create a file"
    );
    assert!(
        !missing_parent_path.exists(),
        "missing-parent rejection must not create a file"
    );
    assert_eq!(
        fs::read(&legacy_path).expect("legacy typed IO output should be readable"),
        b"base"
    );
    assert_eq!(
        fs::read(&after_path).expect("later guest write should succeed"),
        b"after"
    );
    assert_eq!(
        vm.host_context().resource_count(),
        0,
        "unrelated io.file must be closed"
    );
    assert!(
        vm.execution_scope().operations().is_empty(),
        "no IO operation may remain pending"
    );
}

#[test]
fn task7_try_write_text_policy_deny_is_data_and_leaves_existing_bytes() {
    let scratch = ScratchDir::new("write-denied");
    let target = scratch.path().join("denied.txt");
    fs::write(&target, b"unchanged").expect("sentinel should be written");
    let source = format!(
        "use io; pub fn denied_write() {{ let result = io::try_write_text({}, \"changed\"); assert(!result.ok); assert(result.error_code == \"policy_denied\"); 31; }} denied_write();",
        serde_json::to_string(target.to_str().expect("UTF-8 fixture path")).unwrap(),
    );
    let policy = IoPolicy {
        allowed_roots: vec![scratch.path().to_string_lossy().into_owned()],
        allow_write: false,
        allow_process: true,
        max_read_bytes: 1024,
        max_write_bytes: 1024,
    };
    let (mut vm, submitted) = run_standard(&source, Some(policy));
    let pending = run_to_halt(&mut vm).expect("write authority denial must be a typed result");
    assert_backend_driver(&submitted, pending);
    assert_eq!(vm.stack().last(), Some(&Value::Int(31)));
    assert_eq!(
        fs::read(&target).expect("denied target should remain readable"),
        b"unchanged"
    );
    assert_eq!(vm.host_context().resource_count(), 0);
    assert!(vm.execution_scope().operations().is_empty());
}

#[cfg(feature = "async")]
#[test]
fn task7_try_write_text_async_reset_cancels_before_future_poll_without_mutation() {
    let scratch = ScratchDir::new("async-cancel");
    let target = scratch.path().join("sentinel.txt");
    fs::write(&target, b"sentinel").expect("sentinel should be written");
    let path = serde_json::to_string(target.to_str().expect("fixture path is UTF-8")).unwrap();
    let source = format!("use io; io::try_write_text({path}, \"changed\"); 5;");
    let catalog = standard_host_catalog();
    let compiled = compile(&source, Arc::clone(&catalog)).expect("source should compile");
    let mut vm = Vm::try_new(compiled.program).expect("test VM should construct");
    let mut registry = HostFunctionRegistry::empty();
    install_catalog_module(&mut registry, catalog.as_ref(), "io");
    registry
        .bind_vm_cached(&mut vm)
        .expect("standard io module should bind");
    vm.configure_io(IoPolicy {
        allowed_roots: vec![scratch.path().to_string_lossy().into_owned()],
        allow_write: true,
        allow_process: false,
        max_read_bytes: 64,
        max_write_bytes: 64,
    });
    let cancelled = Arc::new(AtomicUsize::new(0));
    let submitted = install_async_driver_with_cancel_tracking(&mut vm, Arc::clone(&cancelled));

    assert!(matches!(
        vm.run()
            .expect("try_write_text should submit its async operation"),
        VmStatus::Waiting(_)
    ));
    assert_eq!(submitted.load(Ordering::SeqCst), 1);
    vm.reset_for_reuse()
        .expect("reset should cancel the pending write");

    assert_eq!(cancelled.load(Ordering::SeqCst), 1);
    assert!(!vm.scope_reset_pending());
    assert_eq!(
        fs::read(&target).expect("cancelled target should remain readable"),
        b"sentinel"
    );
    assert_eq!(vm.host_context().resource_count(), 0);
    assert!(vm.execution_scope().resources().is_empty());
    assert!(vm.execution_scope().operations().is_empty());
}

#[test]
fn task7_wasm_catalog_surface_has_typed_unsupported_implementation() {
    #[cfg(target_arch = "wasm32")]
    {
        let source = include_str!("../src/builtins/runtime/io_wasm/try_write_text.rs");
        assert!(source.contains("failure(\"unsupported\")"));
    }
}

#[test]
fn task7_static_builtin_table_remains_at_the_existing_full_count() {
    let expected = if cfg!(feature = "sqlite") { 109 } else { 104 };
    assert_eq!(vm::BUILTIN_CATALOG.len(), expected);
    assert_eq!(vm::builtin_call_index("json::is_valid"), None);
    assert_eq!(vm::builtin_call_index("io::try_write_text"), None);
}
