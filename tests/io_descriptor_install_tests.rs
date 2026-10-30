//! Runtime evidence for the standard `io` host module.
//!
//! The architecture guard proves the descriptors are *declared* coherently by
//! reading sources. This test proves the descriptor path is *executable*: it
//! installs the composed standard `io` module through its own descriptors and
//! catalog, then runs the adapters that install produced over the
//! ownership-relevant `open`/`write`/`read_all`/`close` path.
//!
//! It covers:
//!
//! * schema/binding/effect coherence of the published contracts, checked
//!   against the module's own catalog and the shared
//!   [`guest_resource_effects`](vm::host_extension::guest_resource_effects)
//!   derivation;
//! * install coherence: a restricted registry that denies every import before
//!   the module installs contains exactly the module's owned functions after;
//! * the real adapter paths, driven by the VM, including a stale-handle
//!   rejection after `close`;
//! * restricted behavior: the IO policy enforced by the installed adapters
//!   rejects a write without the write capability and a path outside the
//!   allowed roots.

#![cfg(feature = "runtime")]

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use vm::{
    HostBindingKind, HostEffect, HostFunctionDescriptor, HostFunctionRegistry, HostTypeSchema,
    IoHostExt, IoPolicy, ResourceEffect, ResourceTypeKey, StandardHostModule, Value, Vm, VmError,
    VmStatus, compile_source,
};

/// The composed standard `io` module for this build.
fn io_module() -> &'static StandardHostModule {
    vm::standard_host_modules()
        .iter()
        .find(|module| module.name == "io")
        .expect("the standard io host module must be composed")
}

/// The module's owned descriptors, keyed by guest name.
fn owned_descriptors() -> BTreeMap<String, HostFunctionDescriptor> {
    io_module()
        .owned_descriptors()
        .into_iter()
        .map(|descriptor| (descriptor.schema.name.clone(), descriptor))
        .collect()
}

/// The standard `io` module as a descriptor module: exactly the functions the
/// standard composition owns for `io`.
fn io_descriptor_module() -> vm::HostModuleDescriptor {
    vm::HostModuleDescriptor {
        name: io_module().name,
        functions: io_module().owned,
        resources: &[],
    }
}

/// Installs the standard `io` module through the descriptor/catalog path into a
/// restricted registry, which grants exactly the module's own imports.
fn installed_restricted_registry() -> HostFunctionRegistry {
    let module = io_descriptor_module();
    let contract = module
        .catalog()
        .expect("the io descriptor catalog must be valid");
    let mut registry = HostFunctionRegistry::restricted();
    // A restricted registry grants its legacy namespaced builtins explicitly.
    // Catalog-only imports are authorized by descriptor installation itself.
    for name in owned_descriptors().keys() {
        if vm::builtin_call_index(name).is_some() {
            registry.allow_builtin(name).unwrap_or_else(|error| {
                panic!("`{name}` must be a known namespaced builtin: {error}")
            });
        }
    }
    module
        .install_from_catalog(&mut registry, &contract)
        .expect("the io module must install from its own catalog");
    registry
}

fn io_file_key() -> ResourceTypeKey {
    ResourceTypeKey::new("io.file").expect("io.file is a valid resource type key")
}

/// The guest resource effect a descriptor must carry for `effect`.
fn guest_effect(effect: ResourceEffect) -> HostEffect {
    HostEffect::GuestResource(effect)
}

fn host_error(error: VmError) -> String {
    match error {
        VmError::HostError(message) => message,
        other => panic!("expected a host error, got {other:?}"),
    }
}

/// A unique, empty scratch directory for one test run.
fn scratch_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should follow the Unix epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "pd-vm-io-descriptor-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("scratch directory should be creatable");
    dir
}

// ---------------------------------------------------------------- descriptors

#[test]
fn io_module_descriptors_carry_coherent_schema_binding_and_effects() {
    let owned = owned_descriptors();
    let mut expected_names = [
        "io::open",
        "io::popen",
        "io::read_all",
        "io::read_line",
        "io::write",
        "io::try_write_text",
        "io::flush",
        "io::close",
        "io::exists",
    ];
    expected_names.sort_unstable();
    let names: Vec<&str> = owned.keys().map(String::as_str).collect();
    assert_eq!(
        names, expected_names,
        "the io module must own exactly the standard IO surface"
    );

    let key = io_file_key();
    let open = &owned["io::open"];
    assert_eq!(
        open.schema.return_type,
        HostTypeSchema::Resource(key.clone()),
        "io::open must return the typed io.file resource"
    );
    assert!(
        open.effects
            .contains(&guest_effect(ResourceEffect::Create { key: key.clone() })),
        "io::open must carry the resource creation effect: {:?}",
        open.effects
    );

    let read_all = &owned["io::read_all"];
    assert_eq!(
        read_all.schema.params.len(),
        1,
        "io::read_all must take exactly the handle"
    );
    assert_eq!(
        read_all.schema.params[0].ty,
        HostTypeSchema::Resource(key.clone()),
        "io::read_all must take the typed io.file resource"
    );
    assert!(
        read_all
            .effects
            .contains(&guest_effect(ResourceEffect::Borrow { key: key.clone() })),
        "io::read_all must borrow the handle: {:?}",
        read_all.effects
    );

    let close = &owned["io::close"];
    assert!(
        close
            .effects
            .contains(&guest_effect(ResourceEffect::TakeOwned {
                key: key.clone()
            })),
        "io::close must take ownership of the handle: {:?}",
        close.effects
    );

    // Every descriptor's effects are the shared derivation of its schema, and
    // every adapter is a registered pending-operation driver, so the binding
    // class is the stack class (not owned dispatch).
    for descriptor in owned.values() {
        assert_eq!(
            descriptor.effects,
            vm::host_extension::guest_resource_effects(&descriptor.schema),
            "`{}` effects must be derived from its own contract",
            descriptor.schema.name
        );
        assert_eq!(
            descriptor.binding.kind,
            HostBindingKind::StaticStack,
            "`{}` must use the stack dispatch class",
            descriptor.schema.name
        );
    }

    // The published catalog includes the resource lifecycle and the
    // resource-free whole-file writer, and declares the resource those
    // lifecycle contracts refer to.
    let catalog = io_module()
        .catalog_module()
        .expect("the io module publishes a catalog surface")
        .catalog()
        .expect("the io catalog is valid");
    let published: Vec<&str> = catalog
        .functions()
        .iter()
        .map(|function| function.name.as_str())
        .collect();
    assert_eq!(
        published,
        [
            "io::open",
            "io::read_all",
            "io::write",
            "io::try_write_text",
            "io::flush",
            "io::close"
        ],
        "the io catalog surface includes open, borrowed read/write/flush, consuming close, and resource-free try_write_text"
    );
    let resources: Vec<(&str, &str)> = catalog
        .resources()
        .iter()
        .map(|resource| (resource.key.as_str(), resource.description.as_str()))
        .collect();
    assert_eq!(
        resources,
        [("io.file", "An open file handle")],
        "the io.file declaration must stay the single catalog resource"
    );
}

// -------------------------------------------------------------------- install

#[test]
fn io_module_install_grants_its_owned_functions_on_a_restricted_registry() {
    let names: Vec<String> = owned_descriptors().keys().cloned().collect();

    let mut registry = HostFunctionRegistry::restricted();
    for name in &names {
        assert!(
            !registry.contains_name(name),
            "a restricted registry must deny `{name}` before the module declares it"
        );
    }

    let module = io_descriptor_module();
    let contract = module
        .catalog()
        .expect("the io descriptor catalog must be valid");
    let catalog = module
        .install_from_catalog(&mut registry, &contract)
        .expect("the io module must install from its own catalog");
    for name in &names {
        assert!(
            registry.contains_name(name),
            "`{name}` must be bound by the descriptor install"
        );
    }
    assert_eq!(
        catalog.functions().len(),
        names.len(),
        "the descriptor install must register every owned function"
    );
}

#[test]
fn io_module_install_rejects_a_descriptor_list_whose_adapters_disagree() {
    // Transactional install: a mismatched binding/adapter pair fails closed and
    // leaves no partially registered io function behind.
    let mut descriptors = io_module().owned_descriptors();
    let last = descriptors.len() - 1;
    descriptors[last].binding.kind = HostBindingKind::Static;
    let mut registry = HostFunctionRegistry::restricted();

    let error = vm::HostModuleDescriptor::install_descriptors(&mut registry, &descriptors)
        .expect_err("a binding/adapter disagreement must fail the whole module");
    assert!(
        error.to_string().contains("binding/adapter mismatch"),
        "{error}"
    );
    for name in owned_descriptors().keys() {
        assert!(
            !registry.contains_name(name),
            "a rejected io install must leave `{name}` unbound"
        );
    }
}

// ------------------------------------------------------------------ execution

/// The IO calls the round-trip script makes. Each one is dispatched through the
/// installed descriptor and must be driven to completion as a pending host
/// operation.
const ROUND_TRIP_IO_CALLS: usize = 7;

/// Submitted-future count of the async host driver; stays zero on the blocking
/// backend, which drives registered operation drivers instead.
type SubmittedOps = std::sync::Arc<std::sync::atomic::AtomicUsize>;

/// One driven run: the final stack plus the number of pending host operations
/// the run loop had to drive to completion.
#[derive(Debug)]
struct DrivenRun {
    stack: Vec<Value>,
    driven_ops: usize,
}

/// Runs the VM, driving every pending host operation exactly the way an embedder
/// does: `run` yields [`VmStatus::Waiting`], the pending operation is resolved,
/// and the VM is resumed.
fn run_driven(vm: &mut Vm) -> Result<DrivenRun, VmError> {
    let mut driven_ops = 0usize;
    let mut status = vm.run()?;
    loop {
        match status {
            VmStatus::Halted => {
                return Ok(DrivenRun {
                    stack: vm.stack().to_vec(),
                    driven_ops,
                });
            }
            VmStatus::Yielded => status = vm.resume()?,
            VmStatus::Waiting(_) => {
                vm.wait_for_host_op_blocking()?;
                driven_ops += 1;
                status = vm.resume()?;
            }
        }
    }
}

/// Installs the async host driver the async IO backend submits its pending
/// operations to, and returns the submitted-operation counter.
///
/// The submitted futures are polled inside an entered tokio runtime: the async
/// backend opens and reads files through tokio, which needs a reactor context.
/// The driver is the *only* way the async backend's pending operations can
/// complete, so a run that finishes under this backend proves the
/// submit/poll/resume lifecycle, not just the adapter call.
#[cfg(feature = "async")]
fn install_async_driver(vm: &mut Vm) -> SubmittedOps {
    use std::collections::HashMap;
    use std::sync::atomic::Ordering;
    use std::task::{Context, Poll};

    use vm::{CallReturn, HostAsyncBridge, HostFuture, HostFutureOutput, HostOpId, VmResult};

    struct TokioHostDriver {
        runtime: tokio::runtime::Runtime,
        submitted: HashMap<HostOpId, HostFuture>,
        count: SubmittedOps,
    }

    impl TokioHostDriver {
        fn new(count: SubmittedOps) -> Self {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("the test runtime should build");
            Self {
                runtime,
                submitted: HashMap::new(),
                count,
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
                let future = match self.submitted.get_mut(&op_id) {
                    Some(future) => future,
                    None => {
                        return Poll::Ready(Err(VmError::HostError(format!(
                            "unknown submitted host operation {op_id}"
                        ))));
                    }
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
    }

    let count = SubmittedOps::default();
    vm.set_async_bridge(Box::new(TokioHostDriver::new(std::sync::Arc::clone(
        &count,
    ))))
    .expect("the test async host driver should install");
    count
}

/// The blocking backend schedules concrete operation drivers in the execution
/// scope, so it submits no futures and needs no bridge.
#[cfg(not(feature = "async"))]
fn install_async_driver(_vm: &mut Vm) -> SubmittedOps {
    SubmittedOps::default()
}

/// Asserts that the pending operations the run loop drove were resolved by the
/// backend that is compiled in: the async backend resolves each one through the
/// submitted-future bridge, the blocking backend through its registered
/// operation driver.
fn assert_pending_ops_resolved_by_backend(submitted: &SubmittedOps, driven_ops: usize) {
    use std::sync::atomic::Ordering;
    let submitted = submitted.load(Ordering::SeqCst);
    #[cfg(feature = "async")]
    {
        assert!(
            driven_ops >= ROUND_TRIP_IO_CALLS,
            "the async backend must yield a pending operation per IO call, drove {driven_ops}"
        );
        assert_eq!(
            submitted, driven_ops,
            "every driven pending operation must be resolved by the submitted-future bridge"
        );
    }
    #[cfg(not(feature = "async"))]
    {
        assert_eq!(submitted, 0, "the blocking backend must not submit futures");
        assert_eq!(
            driven_ops, ROUND_TRIP_IO_CALLS,
            "the blocking backend must drive one registered operation driver per IO call"
        );
    }
}

/// Asserts that a rejected call failed closed and left no IO state behind.
///
/// Both backends must reject a policy violation deterministically and release
/// everything the call touched. Where the rejection happens is the backend's own
/// established behavior: the blocking adapter authorizes the path before it
/// schedules work, while the async adapter resolves the authorization inside the
/// submitted operation and reports the error from it.
fn assert_rejected_call_left_no_io_state(vm: &mut Vm, submitted: &SubmittedOps) {
    assert_eq!(
        vm.host_context().resource_count(),
        0,
        "a rejected io call must not leave a live io.file resource (no usable handle)"
    );
    assert!(
        vm.execution_scope().operations().is_empty(),
        "a rejected io call must not leave a pending operation behind"
    );
    #[cfg(feature = "async")]
    assert!(
        submitted.load(std::sync::atomic::Ordering::SeqCst) >= 1,
        "the async backend submits the operation and then reports the policy error"
    );
    #[cfg(not(feature = "async"))]
    assert_eq!(
        submitted.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the blocking adapter must reject a policy violation before scheduling work"
    );
}

/// A VM whose host functions come **only** from the descriptor install of the
/// standard `io` module, restricted by default, plus the backend's host driver.
fn vm_with_installed_io(source: &str) -> (Vm, SubmittedOps) {
    let compiled = compile_source(&format!("use io;\n{source}")).expect("IO source should compile");
    let mut vm = Vm::new(compiled.program);
    let registry = installed_restricted_registry();
    registry
        .bind_vm_cached(&mut vm)
        .expect("the installed io registry must bind to the VM");
    let submitted = install_async_driver(&mut vm);
    (vm, submitted)
}

#[test]
fn installed_io_descriptors_execute_open_write_read_close() {
    let dir = scratch_dir("roundtrip");
    let inside = dir.join("payload.txt");
    let (mut vm, submitted) = vm_with_installed_io(&format!(
        r#"
        let written = io::open("{inside}", "w");
        io::write(written, "descriptor-installed");
        io::flush(written);
        let closed = io::close(written);
        let read_back = io::open("{inside}", "r");
        let contents = io::read_all(read_back);
        let closed_again = io::close(read_back);
        contents;
        "#,
        inside = inside.display()
    ));
    vm.configure_io(IoPolicy {
        allowed_roots: vec![dir.display().to_string()],
        allow_write: true,
        ..IoPolicy::default()
    });

    let run = run_driven(&mut vm).expect("the descriptor-installed IO path must run");
    assert_eq!(
        run.stack.last(),
        Some(&Value::String(std::sync::Arc::new(
            "descriptor-installed".to_string()
        ))),
        "io::read_all must return what io::write stored"
    );
    assert_eq!(
        fs::read_to_string(&inside).expect("the written file must exist"),
        "descriptor-installed"
    );
    assert_pending_ops_resolved_by_backend(&submitted, run.driven_ops);
    assert_eq!(
        vm.host_context().resource_count(),
        0,
        "every opened io.file handle must be closed by the script"
    );
    assert!(
        vm.execution_scope().operations().is_empty(),
        "a completed IO script must leave no pending operation behind"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn installed_io_descriptors_reject_a_stale_handle_after_close() {
    let dir = scratch_dir("stale");
    let inside = dir.join("payload.txt");
    fs::write(&inside, "ownership").expect("fixture file should be writable");
    let (mut vm, submitted) = vm_with_installed_io(&format!(
        r#"
        let handle = io::open("{inside}", "r");
        let closed = io::close(handle);
        io::read_all(handle);
        "#,
        inside = inside.display()
    ));
    vm.configure_io(IoPolicy {
        allowed_roots: vec![dir.display().to_string()],
        ..IoPolicy::default()
    });

    let error = host_error(run_driven(&mut vm).expect_err("a closed handle must not be readable"));
    assert!(
        error.contains("stale") || error.contains("closed") || error.contains("invalid"),
        "reading a closed handle must be rejected by the installed adapter: {error}"
    );
    // The open and the close completed before the stale read was rejected, so
    // the async backend resolved them through the bridge.
    #[cfg(feature = "async")]
    assert!(
        submitted.load(std::sync::atomic::Ordering::SeqCst) >= 2,
        "the successful prefix of the run must be resolved by the bridge"
    );
    #[cfg(not(feature = "async"))]
    assert_eq!(
        submitted.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the blocking backend must not submit futures"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn installed_io_descriptors_enforce_the_configured_restrictions() {
    let allowed = scratch_dir("allowed");
    let other = scratch_dir("other");
    let outside = other.join("outside.txt");
    fs::write(&outside, "outside").expect("fixture file should be writable");
    let inside = allowed.join("payload.txt");

    // A restricted registry turns the default policy on: no roots, no write.
    // Both backends must reject the write before it reaches the file system.
    let (mut no_write, submitted) = vm_with_installed_io(&format!(
        r#"
        io::open("{inside}", "w");
        "#,
        inside = inside.display()
    ));
    let error = host_error(
        run_driven(&mut no_write).expect_err("a write without the write capability must fail"),
    );
    assert!(
        error.contains("write capability"),
        "the default policy must reject writes: {error}"
    );
    // Fail closed: the rejected write must not reach the file system, whichever
    // backend resolved the call.
    assert!(
        !inside.exists(),
        "a rejected write must not create the target file"
    );
    assert_rejected_call_left_no_io_state(&mut no_write, &submitted);

    let (mut policy, submitted) = vm_with_installed_io(&format!(
        r#"
        io::open("{outside}", "r");
        "#,
        outside = outside.display()
    ));
    policy.configure_io(IoPolicy {
        allowed_roots: vec![allowed.display().to_string()],
        allow_write: true,
        ..IoPolicy::default()
    });
    let error = host_error(
        run_driven(&mut policy).expect_err("a path outside the allowed roots must fail"),
    );
    assert!(
        error.contains("outside the allowed roots"),
        "the configured roots must bound the adapter: {error}"
    );
    assert_rejected_call_left_no_io_state(&mut policy, &submitted);

    let _ = fs::remove_dir_all(&allowed);
    let _ = fs::remove_dir_all(&other);
}

// ---------------------------------------------------- standard catalog contract

/// Installs the standard `io` catalog surface the way an embedder does.
///
/// Guest calls on that surface compile to host imports. Legacy builtins are
/// granted as well so a namespaced fallback cannot hide a missing catalog
/// entry. Catalog-only APIs are authorized by their registered descriptors.
fn installed_standard_catalog_registry() -> HostFunctionRegistry {
    let module = io_module()
        .catalog_module()
        .expect("the io module publishes a catalog surface");
    let catalog = vm::standard_host_catalog();
    let surface = module
        .catalog()
        .expect("the io catalog surface must be valid");
    let mut registry = HostFunctionRegistry::empty();
    registry.set_standard_composition(vm::standard_composition());
    for function in surface.functions() {
        if vm::builtin_call_index(&function.name).is_some() {
            registry
                .allow_builtin(&function.name)
                .unwrap_or_else(|error| {
                    panic!(
                        "`{}` is a legacy io builtin and must be grantable: {error}",
                        function.name
                    )
                });
        }
    }
    module
        .install_from_catalog(&mut registry, catalog.as_ref())
        .expect("the io catalog surface must install from the standard catalog");
    registry
}

fn vm_with_standard_catalog_io(source: &str) -> (Vm, SubmittedOps) {
    let catalog = vm::standard_host_catalog();
    let compiled = vm::compile_source_with_flavor_and_options(
        &format!("use io;\n{source}"),
        vm::SourceFlavor::RustScript,
        vm::CompileSourceFileOptions::new().with_host_api_catalog(std::sync::Arc::clone(&catalog)),
    )
    .unwrap_or_else(|error| panic!("standard-catalog io source failed to compile: {error}"));
    let mut vm = Vm::new(compiled.program);
    vm.set_default_host_fallback_enabled(false);
    vm.set_standard_composition(vm::standard_composition());
    let registry = installed_standard_catalog_registry();
    let surface = io_module()
        .catalog_module()
        .expect("the io module publishes a catalog surface");
    surface
        .install_state_requirements(&mut vm)
        .expect("io catalog state requirements must install");
    registry
        .bind_vm_cached(&mut vm)
        .unwrap_or_else(|error| panic!("standard-catalog io registry failed to bind: {error}"));
    let submitted = install_async_driver(&mut vm);
    (vm, submitted)
}

fn writable_policy(root: &std::path::Path) -> IoPolicy {
    IoPolicy {
        allowed_roots: vec![root.display().to_string()],
        allow_write: true,
        allow_process: false,
        ..IoPolicy::default()
    }
}

/// `io::write` and `io::flush` are part of the published standard catalog.
///
/// Both borrow `io.file`. They are not a second int-handle builtin schema.
#[test]
fn standard_catalog_publishes_borrowed_write_and_flush() {
    let catalog = vm::standard_host_catalog();
    let key = io_file_key();
    for name in ["io::write", "io::flush"] {
        let function = catalog.function(name).unwrap_or_else(|| {
            let owned = owned_descriptors()
                .get(name)
                .map(|descriptor| format!("{:?}", descriptor.schema.params));
            panic!(
                "standard catalog must publish typed `{name}`; owned descriptor params are {owned:?}"
            )
        });
        assert_eq!(
            function.params.first().map(|param| &param.ty),
            Some(&HostTypeSchema::Resource(key.clone())),
            "`{name}` must take the typed io.file resource, got {function:?}"
        );
        assert_eq!(
            function.params.first().map(|param| param.passing),
            Some(vm::HostParamPassing::Borrow),
            "`{name}` must borrow the handle, got {function:?}"
        );
    }
    let write = catalog
        .function("io::write")
        .expect("io::write is published");
    assert_eq!(
        write.params.len(),
        2,
        "io::write takes the handle and the text"
    );
    assert_eq!(write.params[1].ty, HostTypeSchema::String);
    assert_eq!(write.return_type, HostTypeSchema::Int);
    let flush = catalog
        .function("io::flush")
        .expect("io::flush is published");
    assert_eq!(flush.params.len(), 1, "io::flush takes only the handle");
    assert_eq!(flush.return_type, HostTypeSchema::Bool);
}

/// `io::open` through the standard catalog must return a live `io.file`.
#[test]
fn standard_catalog_open_returns_a_stamped_io_file() {
    let dir = scratch_dir("catalog-open");
    let inside = dir.join("opened.txt");
    let (mut vm, _) = vm_with_standard_catalog_io(&format!(
        r#"
        let handle = io::open("{inside}", "w");
        handle;
        "#,
        inside = inside.display()
    ));
    vm.configure_io(writable_policy(&dir));

    let run = run_driven(&mut vm)
        .unwrap_or_else(|error| panic!("standard-catalog io::open failed: {error}"));
    assert!(inside.exists(), "open writable must create the file");
    assert_eq!(
        vm.host_context().resource_count(),
        1,
        "the opened io.file must stay live until close"
    );
    assert!(matches!(run.stack.last(), Some(Value::Int(_))));
    let _ = fs::remove_dir_all(&dir);
}

/// Real file bytes through the standard catalog: open writable, borrowed
/// write, flush, consuming close, reopen, read_all, close.
#[test]
fn standard_catalog_round_trips_file_bytes() {
    let dir = scratch_dir("catalog-roundtrip");
    let inside = dir.join("rss.txt");
    let (mut vm, submitted) = vm_with_standard_catalog_io(&format!(
        r#"
        let written = io::open("{inside}", "w");
        io::write(written, "xehentai-rss-roundtrip");
        io::flush(written);
        let closed = io::close(written);
        let read_back = io::open("{inside}", "r");
        let contents = io::read_all(read_back);
        let closed_again = io::close(read_back);
        contents;
        "#,
        inside = inside.display()
    ));
    vm.configure_io(writable_policy(&dir));

    let run = run_driven(&mut vm)
        .unwrap_or_else(|error| panic!("standard-catalog io.file round trip failed: {error}"));
    assert_eq!(
        run.stack.last(),
        Some(&Value::String(std::sync::Arc::new(
            "xehentai-rss-roundtrip".to_string()
        )))
    );
    assert_eq!(
        fs::read_to_string(&inside).expect("the written file must exist"),
        "xehentai-rss-roundtrip"
    );
    assert_pending_ops_resolved_by_backend(&submitted, run.driven_ops);
    assert_eq!(
        vm.host_context().resource_count(),
        0,
        "close must release every io.file resource"
    );
    assert!(vm.execution_scope().operations().is_empty());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn standard_catalog_rejects_write_outside_policy() {
    let allowed = scratch_dir("catalog-allowed");
    let other = scratch_dir("catalog-other");
    let outside = other.join("outside.txt");
    fs::write(&outside, "outside").expect("fixture file should be writable");
    let inside = allowed.join("payload.txt");

    let (mut no_write, submitted) = vm_with_standard_catalog_io(&format!(
        r#"
        io::open("{inside}", "w");
        "#,
        inside = inside.display()
    ));
    no_write.configure_io(IoPolicy {
        allowed_roots: vec![allowed.display().to_string()],
        allow_write: false,
        allow_process: false,
        ..IoPolicy::default()
    });
    let error = host_error(
        run_driven(&mut no_write).expect_err("a write without the write capability must fail"),
    );
    assert!(
        error.contains("write capability"),
        "write policy must reject the open: {error}"
    );
    assert!(
        !inside.exists(),
        "a rejected write must not create the file"
    );
    assert_rejected_call_left_no_io_state(&mut no_write, &submitted);

    let (mut wrong_root, submitted) = vm_with_standard_catalog_io(&format!(
        r#"
        io::open("{outside}", "r");
        "#,
        outside = outside.display()
    ));
    wrong_root.configure_io(IoPolicy {
        allowed_roots: vec![allowed.display().to_string()],
        allow_write: true,
        allow_process: false,
        ..IoPolicy::default()
    });
    let error = host_error(
        run_driven(&mut wrong_root).expect_err("a path outside the allowed roots must fail"),
    );
    assert!(
        error.contains("outside the allowed roots"),
        "roots must bound the catalog adapter: {error}"
    );
    assert_rejected_call_left_no_io_state(&mut wrong_root, &submitted);

    let _ = fs::remove_dir_all(&allowed);
    let _ = fs::remove_dir_all(&other);
}

#[test]
fn standard_catalog_rejects_a_stale_handle_and_a_second_close() {
    let dir = scratch_dir("catalog-stale");
    let inside = dir.join("payload.txt");
    fs::write(&inside, "ownership").expect("fixture file should be writable");

    for source in [
        format!(
            "let handle = io::open(\"{}\", \"r\"); io::close(handle); io::read_all(handle);",
            inside.display()
        ),
        format!(
            "let handle = io::open(\"{}\", \"r\"); io::close(handle); io::close(handle);",
            inside.display()
        ),
    ] {
        let compiled = vm::compile_source_with_flavor_and_options(
            &format!("use io;\n{source}"),
            vm::SourceFlavor::RustScript,
            vm::CompileSourceFileOptions::new().with_host_api_catalog(vm::standard_host_catalog()),
        );
        let Err(error) = compiled else {
            panic!("a consumed io.file must not be usable again");
        };
        let error = error.to_string();
        assert!(
            error.contains("moved"),
            "consuming close must make a second use ill-typed: {error}"
        );
    }

    let (mut closed, _submitted) = vm_with_standard_catalog_io(&format!(
        r#"
        let handle = io::open("{inside}", "r");
        io::close(handle);
        "#,
        inside = inside.display()
    ));
    closed.configure_io(writable_policy(&dir));
    run_driven(&mut closed).expect("consuming close must succeed");
    assert_eq!(closed.host_context().resource_count(), 0);
    assert!(closed.execution_scope().operations().is_empty());
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn standard_catalog_registry_keeps_process_off() {
    let compiled = compile_source(
        r#"
        use io;
        io::popen("exit 0", "r");
        "#,
    )
    .expect("popen source should compile");
    let mut registry = installed_standard_catalog_registry();
    registry
        .allow_builtin("io::popen")
        .expect("io::popen remains a namespaced builtin");
    let mut vm = Vm::new(compiled.program);
    vm.set_default_host_fallback_enabled(false);
    vm.configure_io(IoPolicy {
        allow_process: false,
        ..IoPolicy::default()
    });
    registry
        .bind_vm_cached(&mut vm)
        .expect("catalog registry plus the popen builtin should bind");
    let _submitted = install_async_driver(&mut vm);

    let error = host_error(run_driven(&mut vm).expect_err("process launch must stay denied"));
    assert!(
        error.contains("process capability") || error.contains("command capability"),
        "process stays off unless the policy grants it: {error}"
    );
    assert_eq!(vm.host_context().resource_count(), 0);
}
