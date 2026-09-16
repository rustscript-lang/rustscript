//! External host-extension fixture.
//!
//! A standalone crate that consumes only the **public** host-extension SDK of
//! `pd-vm` (crate name `vm`): the [`HostApiCatalog`] model, typed per-VM
//! module state through [`HostContext`], the [`HostExtension`] register /
//! install lifecycle, external [`HostResource`] insertion and typed borrows
//! through the generic host boundary, and external concrete
//! [`HostOperation`] drivers started into the VM's execution scope.
//!
//! It is deliberately **not** a member of the pd-vm workspace: this proves
//! the extension surface works from a genuinely separate crate with no
//! crate-private access.

use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use pd_host_function::pd_host_function;

#[cfg(test)]
use vm::{BytecodeBuilder, HostContextErrorKind, HostImport};
use vm::{
    HostApiCatalog, HostContextError, HostExtension, HostFunctionRegistry, HostModuleDescriptor,
    HostParamPassing, HostResourceType, HostResourceTypeMeta, HostTypeSchema, ResourceTypeKey,
    Value, Vm, VmError, VmResult, arg, borrow_arg, catalog_import_schemas, resource,
};

/// Number of times the external `Counter` resource was closed.
pub static CLOSED_COUNTERS: AtomicUsize = AtomicUsize::new(0);
/// Number of times the external `Widget` resource was closed.
pub static CLOSED_WIDGETS: AtomicUsize = AtomicUsize::new(0);
/// Number of calls that reached the macro-generated keyed handlers.
pub static MACRO_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Serializes tracker-dependent tests (the close counters are process-global).
#[cfg(test)]
static TRACKER_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
fn reset_trackers() {
    CLOSED_COUNTERS.store(0, Ordering::SeqCst);
    CLOSED_WIDGETS.store(0, Ordering::SeqCst);
}

/// A typed external resource: closed through the generic poll-based close
/// contract, identified by a catalog `ResourceTypeKey`.
#[derive(Debug)]
pub struct Counter(pub u64);

impl resource::HostResource for Counter {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(ResourceTypeKey::new("demo.counter").expect("static key"))
    }

    fn begin_close(
        &mut self,
        _reason: resource::ResourceCloseReason,
    ) -> resource::ResourceResult<resource::CloseProgress> {
        CLOSED_COUNTERS.fetch_add(1, Ordering::SeqCst);
        Ok(resource::CloseProgress::Ready)
    }
}

impl HostResourceType for Counter {
    const KEY: &'static str = "demo.counter";
    const DESCRIPTION: &'static str = "An external counter resource";
}

/// A second typed external resource with its own key.
#[derive(Debug)]
pub struct Widget(pub i64);

impl resource::HostResource for Widget {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(ResourceTypeKey::new("demo.widget").expect("static key"))
    }

    fn begin_close(
        &mut self,
        _reason: resource::ResourceCloseReason,
    ) -> resource::ResourceResult<resource::CloseProgress> {
        CLOSED_WIDGETS.fetch_add(1, Ordering::SeqCst);
        Ok(resource::CloseProgress::Ready)
    }
}

impl HostResourceType for Widget {
    const KEY: &'static str = "demo.widget";
    const DESCRIPTION: &'static str = "An external widget resource";
}

/// Persistent per-VM module state: survives execution-scope reset and never
/// participates in resource close. Covered by `HostModule`'s blanket impl.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DemoPolicy {
    pub max_counters: u64,
}

/// An external concrete [`HostOperation`] driver. Polling advances the
/// operation; cancellation records the reason and completes promptly.
#[derive(Debug)]
pub struct CounterOp {
    pub remaining: u64,
    pub cancelled: Arc<AtomicUsize>,
    pub quiescent: bool,
}

impl vm::operation::HostOperation for CounterOp {
    fn poll(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<vm::operation::OperationResult<()>> {
        if self.remaining == 0 {
            std::task::Poll::Ready(Ok(()))
        } else {
            self.remaining -= 1;
            std::task::Poll::Pending
        }
    }

    fn cancel(
        &mut self,
        _reason: vm::operation::OperationCancelReason,
    ) -> vm::operation::OperationResult<()> {
        self.cancelled.fetch_add(1, Ordering::SeqCst);
        self.remaining = 0;
        self.quiescent = true;
        Ok(())
    }

    fn is_quiescent(&self) -> bool {
        self.quiescent
    }
}

// ---- catalog ---------------------------------------------------------------

fn counter_resource() -> HostResourceTypeMeta {
    HostResourceTypeMeta::of::<Counter>()
}

fn widget_resource() -> HostResourceTypeMeta {
    HostResourceTypeMeta::of::<Widget>()
}

fn host_error(error: HostContextError) -> VmError {
    VmError::HostError(error.to_string())
}

mod macro_functions_parent {
    use super::*;

    pub mod functions {
        use super::*;

        /// Inserts a `Counter` into the VM's execution scope and returns its handle.
        #[pd_host_function(name = "demo::make_counter")]
        pub fn make_counter(vm: &mut Vm, seed: i64) -> VmResult<i64> {
            let token = vm
                .host_context()
                .push_resource(Counter(seed as u64))
                .map_err(host_error)?;
            Ok(token.handle().raw() as i64)
        }

        /// Inserts a `Widget` into the VM's execution scope and returns its handle.
        #[pd_host_function(name = "demo::make_widget")]
        pub fn make_widget(vm: &mut Vm, seed: i64) -> VmResult<i64> {
            let token = vm
                .host_context()
                .push_resource(Widget(seed))
                .map_err(host_error)?;
            Ok(token.handle().raw() as i64)
        }

        /// Borrows a `Counter` through the typed host boundary and reads its value.
        #[pd_host_function(name = "demo::read_counter")]
        pub fn read_counter(
            #[pd_host_resource(passing = "borrow", key = "demo.counter")]
            handle: resource::ResourceRef<'_, Counter>,
        ) -> VmResult<i64> {
            Ok(handle.0 as i64)
        }

        /// Starts a concrete [`vm::operation::HostOperation`] driver and returns its id.
        #[pd_host_function(name = "demo::spawn_op")]
        pub fn spawn_op(vm: &mut Vm) -> VmResult<i64> {
            let cancelled = Arc::new(AtomicUsize::new(0));
            let spec = vm::operation::OperationSpec::new(CounterOp {
                remaining: 2,
                cancelled: Arc::clone(&cancelled),
                quiescent: false,
            });
            let id = vm.host_context().start_operation(spec).map_err(host_error)?;
            Ok(id.raw() as i64)
        }

        /// Integer overload of `demo::overloaded`.
        #[pd_host_function(name = "demo::overloaded")]
        pub fn overloaded_int(value: i64) -> i64 {
            let _ = value;
            101
        }

        /// String overload of `demo::overloaded`.
        #[pd_host_function(name = "demo::overloaded")]
        pub fn overloaded_string(value: String) -> String {
            let _ = value;
            "string overload".to_string()
        }

        /// External proc-macro function with a matching borrowed resource key.
        #[pd_host_function(name = "demo::macro_borrow_counter")]
        pub fn macro_borrow_counter(
            #[pd_host_resource(passing = "borrow", key = "demo.counter")]
            counter: resource::ResourceRef<'_, Counter>,
        ) -> VmResult<i64> {
            MACRO_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(counter.0 as i64)
        }

        /// External proc-macro function with a matching owned resource key.
        #[pd_host_function(name = "demo::macro_take_counter")]
        pub fn macro_take_counter(
            #[pd_host_resource(passing = "take_owned", key = "demo.counter")]
            counter: resource::ResourceOwned<Counter>,
        ) -> VmResult<i64> {
            MACRO_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(counter.into_inner().0 as i64)
        }

        /// Deliberately advertises a key different from `Counter`'s concrete key.
        #[pd_host_function(name = "demo::macro_borrow_counter_wrong_key")]
        pub fn macro_borrow_counter_wrong_key(
            #[pd_host_resource(passing = "borrow", key = "demo.wrong")]
            counter: resource::ResourceRef<'_, Counter>,
        ) -> VmResult<i64> {
            MACRO_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(counter.0 as i64)
        }

        /// Deliberately advertises a wrong key on an owned-resource path.
        #[pd_host_function(name = "demo::macro_take_counter_wrong_key")]
        pub fn macro_take_counter_wrong_key(
            #[pd_host_resource(passing = "take_owned", key = "demo.wrong")]
            counter: resource::ResourceOwned<Counter>,
        ) -> VmResult<i64> {
            MACRO_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(counter.into_inner().0 as i64)
        }
    }
}

/// Explicit ordered demo module. Function schemas and adapters come from
/// `#[pd_host_function]`; extra resources keep widget/counter documentation.
pub fn demo_module() -> HostModuleDescriptor {
    HostModuleDescriptor {
        name: "demo",
        functions: &[
            macro_functions_parent::functions::make_counter_descriptor,
            macro_functions_parent::functions::make_widget_descriptor,
            macro_functions_parent::functions::read_counter_descriptor,
            macro_functions_parent::functions::spawn_op_descriptor,
            macro_functions_parent::functions::overloaded_int_descriptor,
            macro_functions_parent::functions::overloaded_string_descriptor,
            macro_functions_parent::functions::macro_borrow_counter_descriptor,
            macro_functions_parent::functions::macro_take_counter_descriptor,
        ],
        resources: &[counter_resource, widget_resource],
    }
}

/// The external extension's catalog: derived from [`demo_module`].
pub fn demo_catalog() -> Arc<HostApiCatalog> {
    Arc::new(demo_module().catalog().expect("catalog must build"))
}

/// External host extension: registers host functions and installs persistent
/// per-VM module state through the public [`HostExtension`] surface.
pub struct DemoExtension;

impl HostExtension for DemoExtension {
    fn register(&self, registry: &mut HostFunctionRegistry) -> VmResult<()> {
        demo_module().install(registry).map(|_| ())
    }

    fn install(&self, vm: &mut Vm) {
        let mut context = vm.host_context();
        context.set_module_state(DemoPolicy { max_counters: 3 });
    }
}

// ---- tests ----------------------------------------------------------------

#[cfg(test)]
fn installed_vm() -> Vm {
    let program = vm::Program::new(Vec::new(), vec![vm::OpCode::Ret as u8]);
    let mut vm = Vm::new(program);
    vm.install_extension(&DemoExtension)
        .expect("extension should install");
    vm
}

#[test]
fn external_scope_state_api_is_typed_and_scope_local() {
    let mut vm = installed_vm();
    assert_eq!(
        vm.host_context().scope_phase(),
        vm::execution_scope::ScopeState::Active
    );

    {
        let mut context = vm.host_context();
        let state = context
            .scope_state_or_insert_with(|| 10_u64)
            .expect("insert typed scope state");
        *state += 2;
    }
    assert_eq!(vm.host_context().scope_state::<u64>(), Some(&12));

    *vm
        .host_context()
        .scope_state_mut::<u64>()
        .expect("mutable typed scope state") += 5;
    assert_eq!(vm.host_context().scope_state::<u64>(), Some(&17));
    assert_eq!(vm.host_context().take_scope_state::<u64>(), Some(17));
    assert_eq!(vm.host_context().scope_state::<u64>(), None);
    assert_eq!(
        vm.host_context().scope_phase(),
        vm::execution_scope::ScopeState::Active
    );
}

#[test]
fn extension_installs_module_state_and_registers_host_functions() {
    let mut vm = installed_vm();
    // The infallible install phase registered typed per-VM module state.
    assert_eq!(
        vm.host_context()
            .module_state::<DemoPolicy>()
            .map(|policy| policy.max_counters),
        Some(3)
    );
    // The catalog-derived registration surface is exercised by register().
    assert!(vm.host_context().is_scope_active());
    drop(vm);
}

#[test]
fn external_resource_insert_borrow_and_close_through_scope() {
    let _tracker_guard = TRACKER_LOCK.lock().unwrap();
    reset_trackers();
    let mut vm = installed_vm();
    let counter = {
        let token = vm
            .host_context()
            .push_resource(Counter(7))
            .expect("insert counter");
        token
    };
    assert_eq!(vm.host_context().resource_count(), 1);

    // Typed borrow reads the value through the SDK (borrow is call-scoped).
    {
        let context = vm.host_context();
        let borrowed = context
            .borrow_resource::<Counter>(counter.handle())
            .expect("borrow counter");
        assert_eq!(borrowed.0, 7);
    }

    // Mut borrow writes through the SDK.
    {
        let mut context = vm.host_context();
        let mut borrowed = context
            .borrow_resource_mut::<Counter>(counter.handle())
            .expect("mut borrow counter");
        borrowed.0 = 9;
    }
    {
        let context = vm.host_context();
        let borrowed = context
            .borrow_resource::<Counter>(counter.handle())
            .expect("re-borrow counter");
        assert_eq!(borrowed.0, 9);
    }

    // Vm drop closes the resource through the scope close sweep.
    drop(vm);
    assert_eq!(CLOSED_COUNTERS.load(Ordering::SeqCst), 1);
}

#[test]
fn typed_wrong_resource_rejection_is_structured() {
    let _tracker_guard = TRACKER_LOCK.lock().unwrap();
    reset_trackers();
    let mut vm = installed_vm();
    let token = vm
        .host_context()
        .push_resource(Widget(2))
        .expect("insert widget");

    // Wrong concrete type is rejected with a structured resource error.
    let error = vm
        .host_context()
        .borrow_resource::<Counter>(token.handle())
        .unwrap_err();
    assert_eq!(error.namespace(), "host::resource");
    assert!(matches!(
        error.kind(),
        HostContextErrorKind::Resource(resource_error)
            if resource_error.code() == resource::ResourceErrorCode::ResourceTypeMismatch
    ));
    drop(vm);
}

#[test]
fn reset_driven_scope_cleanup_closes_resources_and_cancels_operations() {
    let _tracker_guard = TRACKER_LOCK.lock().unwrap();
    reset_trackers();
    let mut vm = installed_vm();
    vm.host_context()
        .push_resource(Counter(1))
        .expect("counter");
    vm.host_context().push_resource(Widget(2)).expect("widget");
    let cancelled = Arc::new(AtomicUsize::new(0));
    let spec = vm::operation::OperationSpec::new(CounterOp {
        remaining: 200,
        cancelled: Arc::clone(&cancelled),
        quiescent: false,
    });
    vm.host_context().start_operation(spec).expect("op start");
    assert_eq!(vm.host_context().resource_count(), 2);
    assert_eq!(vm.host_context().operation_count(), 1);

    // Reset drives the scope to quiescence: resources close, op cancels.
    let _ = vm.reset_for_reuse();
    assert_eq!(vm.host_context().resource_count(), 0);
    assert_eq!(vm.host_context().operation_count(), 0);
    assert_eq!(CLOSED_COUNTERS.load(Ordering::SeqCst), 1);
    assert_eq!(CLOSED_WIDGETS.load(Ordering::SeqCst), 1);
    assert!(
        cancelled.load(Ordering::SeqCst) > 0,
        "the pending operation driver must be cancelled by the scope close"
    );
}

#[test]
fn module_state_survives_reset_and_never_participates_in_close() {
    let _tracker_guard = TRACKER_LOCK.lock().unwrap();
    reset_trackers();
    let mut vm = installed_vm();
    let _ = vm.reset_for_reuse();
    assert!(
        vm.host_context().module_state::<DemoPolicy>().is_some(),
        "module state must survive reset"
    );
    assert_eq!(CLOSED_COUNTERS.load(Ordering::SeqCst), 0);
    assert_eq!(CLOSED_WIDGETS.load(Ordering::SeqCst), 0);
    drop(vm);
}

#[test]
fn catalog_validates_declarations_and_fingerprint() {
    let catalog = demo_catalog();
    assert!(catalog.has_resource(&ResourceTypeKey::new("demo.counter").expect("key")));
    assert!(catalog.has_resource(&ResourceTypeKey::new("demo.widget").expect("key")));

    let make_counter = catalog_import_schemas(&catalog, "demo::make_counter");
    assert_eq!(make_counter.len(), 1);
    assert_eq!(make_counter[0].params.len(), 1);
    assert_eq!(make_counter[0].params[0].name, "seed");
    assert_eq!(make_counter[0].params[0].schema, HostTypeSchema::Int);
    assert_eq!(make_counter[0].params[0].passing, HostParamPassing::Value);
    assert_eq!(make_counter[0].return_type, HostTypeSchema::Int);

    let read_counter = catalog_import_schemas(&catalog, "demo::read_counter");
    assert_eq!(read_counter.len(), 1);
    assert_eq!(read_counter[0].params.len(), 1);
    assert_eq!(
        read_counter[0].params[0].schema,
        HostTypeSchema::Resource(ResourceTypeKey::new("demo.counter").expect("key"))
    );
    assert_eq!(read_counter[0].params[0].passing, HostParamPassing::Borrow);
    assert_eq!(read_counter[0].return_type, HostTypeSchema::Int);

    let mut registry = HostFunctionRegistry::empty();
    DemoExtension
        .register(&mut registry)
        .expect("catalog-backed registration succeeds");
    assert!(registry.contains_name("demo::make_counter"));
    assert!(registry.contains_name("demo::read_counter"));

    // Fingerprint is deterministic.
    assert_eq!(catalog.fingerprint(), catalog.fingerprint());
}

#[test]
fn external_proc_macro_resource_keys_are_checked_before_handler_and_lifecycle_logic() {
    let _tracker_guard = TRACKER_LOCK.lock().unwrap();
    reset_trackers();
    MACRO_HANDLER_CALLS.store(0, Ordering::SeqCst);
    let mut vm = installed_vm();
    let token = vm
        .host_context()
        .push_resource(Counter(37))
        .expect("counter");
    let args = [Value::Int(token.handle().raw() as i64)];

    assert_eq!(
        macro_functions_parent::functions::macro_borrow_counter(&mut vm, &args)
            .expect("matching borrowed key"),
        37
    );
    assert_eq!(MACRO_HANDLER_CALLS.load(Ordering::SeqCst), 1);

    let mismatch = macro_functions_parent::functions::macro_borrow_counter_wrong_key(&mut vm, &args)
        .expect_err("wrong borrowed key must fail before handler");
    assert!(mismatch.to_string().contains("resource type key"));
    assert_eq!(MACRO_HANDLER_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(vm.host_context().resource_count(), 1);

    let owned_mismatch =
        macro_functions_parent::functions::macro_take_counter_wrong_key(&mut vm, &args)
            .expect_err("wrong owned key must fail before take");
    assert!(owned_mismatch.to_string().contains("resource type key"));
    assert_eq!(MACRO_HANDLER_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(vm.host_context().resource_count(), 1);

    assert_eq!(
        macro_functions_parent::functions::macro_take_counter(&mut vm, &args)
            .expect("matching owned key"),
        37
    );
    assert_eq!(MACRO_HANDLER_CALLS.load(Ordering::SeqCst), 2);
    assert_eq!(vm.host_context().resource_count(), 0);
}

#[test]
fn external_catalog_overloads_bind_by_schema_and_fingerprint() {
    let catalog = demo_catalog();
    let schemas = catalog_import_schemas(&catalog, "demo::overloaded");
    assert_eq!(schemas.len(), 2);
    assert!(schemas.iter().all(|schema| schema.fingerprint == catalog.fingerprint()));
    assert!(schemas.iter().all(|schema| schema.name == "demo::overloaded"));

    let integer_schema = schemas
        .iter()
        .find(|schema| schema.params[0].schema == HostTypeSchema::Int)
        .expect("integer overload")
        .clone();
    let string_schema = schemas
        .iter()
        .find(|schema| schema.params[0].schema == HostTypeSchema::String)
        .expect("string overload")
        .clone();
    let mut bytecode = BytecodeBuilder::new();
    bytecode.ldc(0);
    bytecode.call(0, 1);
    bytecode.ldc(1);
    bytecode.call(1, 1);
    bytecode.ret();
    let program = vm::Program::with_imports_and_debug(
        vec![Value::Int(4), Value::string("x")],
        bytecode.finish(),
        vec![
            HostImport {
                name: "demo::overloaded".to_string(),
                arity: 1,
                return_type: vm::ValueType::Int,
            },
            HostImport {
                name: "demo::overloaded".to_string(),
                arity: 1,
                return_type: vm::ValueType::String,
            },
        ],
        None,
    )
    .with_host_import_schemas(vec![integer_schema, string_schema])
    .expect("schema metadata aligns with imports");
    let mut vm = Vm::new(program);
    let mut registry = HostFunctionRegistry::empty();
    DemoExtension
        .register(&mut registry)
        .expect("external extension registration");
    registry
        .bind_vm_cached(&mut vm)
        .expect("both overloads bind");
    assert_eq!(vm.run().expect("overloads execute"), vm::VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::Int(101), Value::string("string overload")]
    );
}
