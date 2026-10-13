#![cfg(feature = "cli")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use vm::cli::{CliHostServices, CliRuntime, run_with_args};
use vm::resource::{HostResource, ResourceHandle};
use vm::{
    CallOutcome, HostApiBuilder, HostFunctionRegistry, HostFunctionSchema, HostParamPassing,
    HostParamSchema, HostTypeSchema, ResourceTypeKey, ResourceTypeSchema, Value, Vm, VmError,
};

#[derive(Clone, Default)]
struct Audit {
    values: Arc<Mutex<Vec<i64>>>,
    resources_dropped: Arc<AtomicUsize>,
    states_dropped: Arc<AtomicUsize>,
    factories: Arc<AtomicUsize>,
}
struct Owner {
    audit: Audit,
    next: i64,
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.audit.states_dropped.fetch_add(1, Ordering::SeqCst);
    }
}
struct Counter {
    value: i64,
    audit: Audit,
}
impl HostResource for Counter {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(key())
    }
}
impl Drop for Counter {
    fn drop(&mut self) {
        self.audit.resources_dropped.fetch_add(1, Ordering::SeqCst);
    }
}
fn key() -> ResourceTypeKey {
    ResourceTypeKey::new("embedding.counter").unwrap()
}
fn error(e: impl std::fmt::Display) -> VmError {
    VmError::HostError(e.to_string())
}
fn handle(args: &[Value]) -> vm::VmResult<ResourceHandle> {
    let Value::Int(raw) = args[0] else {
        return Err(VmError::TypeMismatch("resource"));
    };
    ResourceHandle::from_raw(raw as u64).map_err(error)
}
fn make(vm: &mut Vm, _: &[Value]) -> vm::VmResult<CallOutcome> {
    let (value, audit) = {
        let mut context = vm.host_context();
        let owner = context.module_state_mut::<Owner>().unwrap();
        owner.next += 1;
        (owner.next, owner.audit.clone())
    };
    let token = vm
        .host_context()
        .push_resource(Counter { value, audit })
        .map_err(error)?;
    Ok(CallOutcome::Return(vm::return_one(
        token.handle().raw() as i64
    )))
}
fn increment(vm: &mut Vm, args: &[Value]) -> vm::VmResult<CallOutcome> {
    vm.host_context()
        .borrow_resource_mut::<Counter>(handle(args)?)
        .map_err(error)?
        .value += 1;
    Ok(CallOutcome::Return(vm::return_one(())))
}
fn read(vm: &mut Vm, args: &[Value]) -> vm::VmResult<CallOutcome> {
    let context = vm.host_context();
    let counter = context
        .borrow_resource::<Counter>(handle(args)?)
        .map_err(error)?;
    counter.audit.values.lock().unwrap().push(counter.value);
    Ok(CallOutcome::Return(vm::return_one(counter.value)))
}
fn abort(_: &mut Vm, _: &[Value]) -> vm::VmResult<CallOutcome> {
    Err(error("counter operation failed"))
}

fn services(audit: Audit, fail_init: bool, reset_probe: bool) -> CliHostServices<'static> {
    let resource = HostTypeSchema::Resource(key());
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(key(), "An embedding-owned counter"));
    builder.function(HostFunctionSchema::with_return(
        "counter::make",
        vec![],
        resource.clone(),
    ));
    builder.function(HostFunctionSchema::with_return(
        "counter::increment",
        vec![HostParamSchema::with_passing(
            "counter",
            resource.clone(),
            HostParamPassing::BorrowMut,
        )],
        HostTypeSchema::Null,
    ));
    builder.function(HostFunctionSchema::with_return(
        "counter::read",
        vec![HostParamSchema::with_passing(
            "counter",
            resource,
            HostParamPassing::Borrow,
        )],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "counter::abort",
        vec![],
        HostTypeSchema::Null,
    ));
    let catalog = Arc::new(builder.build().unwrap());
    let register_catalog = Arc::clone(&catalog);
    let factory_audit = audit.clone();
    CliHostServices::new(catalog, move || {
        factory_audit.factories.fetch_add(1, Ordering::SeqCst);
        let mut registry = HostFunctionRegistry::new();
        for (name, function) in [
            ("counter::make", make as vm::StaticHostFunction),
            ("counter::increment", increment),
            ("counter::read", read),
            ("counter::abort", abort),
        ] {
            registry
                .register_catalog_static(
                    vm::catalog_import_schemas(&register_catalog, name).remove(0),
                    function,
                )
                .map_err(error)?;
        }
        Ok(registry)
    })
    .with_vm_init(move |vm| {
        vm.host_context().set_module_state(Owner {
            audit: audit.clone(),
            next: 10,
        });
        if reset_probe || fail_init {
            vm.host_context()
                .push_resource(Counter {
                    value: 99,
                    audit: audit.clone(),
                })
                .map_err(error)?;
        }
        if fail_init {
            return Err(error("initialization failed after allocation"));
        }
        if reset_probe {
            vm.reset_for_reuse()?;
            assert_eq!(vm.host_context().resource_count(), 0);
        }
        Ok(())
    })
}

struct Source(std::path::PathBuf);
impl Source {
    fn new(name: &str, source: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "embedding-resource-{name}-{}.rss",
            std::process::id()
        ));
        std::fs::write(&path, source).unwrap();
        Self(path)
    }
    fn run(&self, services: &CliHostServices<'_>) -> Result<(), Box<dyn std::error::Error>> {
        run_with_args(
            &CliRuntime::default(),
            &[self.0.display().to_string()],
            services,
        )
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn independent_vm_factories_execute_typed_resource_operations_and_drop_once() {
    let audit = Audit::default();
    let configured = services(audit.clone(), false, false);
    let source = Source::new(
        "isolation",
        "let mut counter = counter::make(); counter::increment(&mut counter); counter::read(&counter);",
    );
    source.run(&configured).unwrap();
    source.run(&configured).unwrap();
    assert_eq!(*audit.values.lock().unwrap(), vec![12, 12]);
    assert_eq!(audit.factories.load(Ordering::SeqCst), 2);
    assert_eq!(audit.resources_dropped.load(Ordering::SeqCst), 2);
    assert_eq!(audit.states_dropped.load(Ordering::SeqCst), 2);
}

#[test]
fn failed_initialization_cleans_resources_without_guest_effects() {
    let audit = Audit::default();
    let source = Source::new(
        "failed-init",
        "let counter = counter::make(); counter::read(&counter);",
    );
    let error = source
        .run(&services(audit.clone(), true, false))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("initialization failed after allocation")
    );
    assert!(audit.values.lock().unwrap().is_empty());
    assert_eq!(audit.resources_dropped.load(Ordering::SeqCst), 1);
    assert_eq!(audit.states_dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn guest_host_error_prevents_later_effects_and_cleans_resources() {
    let audit = Audit::default();
    let source = Source::new(
        "guest-error",
        "let counter = counter::make(); counter::abort(); counter::read(&counter);",
    );
    let error = source
        .run(&services(audit.clone(), false, false))
        .unwrap_err();
    assert!(error.to_string().contains("counter operation failed"));
    assert!(audit.values.lock().unwrap().is_empty());
    assert_eq!(audit.resources_dropped.load(Ordering::SeqCst), 1);
    assert_eq!(audit.states_dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn initialization_can_reset_resources_without_sharing_owner_state() {
    let audit = Audit::default();
    let source = Source::new(
        "reset",
        "let counter = counter::make(); counter::read(&counter);",
    );
    source.run(&services(audit.clone(), false, true)).unwrap();
    assert_eq!(*audit.values.lock().unwrap(), vec![11]);
    assert_eq!(audit.resources_dropped.load(Ordering::SeqCst), 2);
    assert_eq!(audit.states_dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn repl_catalog_does_not_bypass_resource_copy_or_strict_typing_rejections() {
    let configured = services(Audit::default(), false, false);
    let options = configured
        .compile_options(vm::CompileSourceFileOptions::default())
        .unwrap();
    let copied = vm::compile_source_for_repl_with_state_and_options(
        "let counter = counter::make(); let alias = counter.copy();",
        &[],
        options.clone(),
    );
    let copied = copied
        .err()
        .expect("resource duplication remains forbidden");
    assert!(
        matches!(&copied, vm::SourceError::Parse(parse) if parse.code.as_deref() == Some("E_OWNERSHIP_COPY_RESOURCE")),
        "{copied}"
    );
    let wrong = vm::compile_source_for_repl_with_state_and_options(
        "fn invalid() -> int { \"wrong\" } invalid();",
        &[],
        options.clone(),
    )
    .err()
    .expect("independent strict rejection");
    assert!(
        matches!(
            &wrong,
            vm::SourceError::Compile(vm::CompileError::StrictTypingRequired { .. })
        ),
        "{wrong}"
    );
    let moved = vm::ReplLocalState {
        binding: vm::ReplLocalBinding {
            name: "message".into(),
            mutable: false,
            schema: Some(vm::TypeSchema::String),
            optional: false,
        },
        moved: true,
    };
    let unavailable =
        vm::compile_source_for_repl_with_state_and_options("message;", &[moved], options)
            .err()
            .expect("independent moved-local rejection");
    assert!(
        matches!(&unavailable, vm::SourceError::Parse(parse) if parse.code.as_deref() == Some("E_LOCAL_MOVED")),
        "{unavailable}"
    );
}
