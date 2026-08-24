//! C2-C1 VM owned-resource local release + exact host-return ownership
//! transfer + native gate tests.
//!
//! Scope:
//! 1. Exact `Resource` host returns transfer HostOwned -> GuestOwned in the
//!    current execution scope (sync and async), before any stack mutation;
//!    foreign/stale/already-guest/taken/closing returns are structured
//!    errors and leave the pre-call stack untouched; legacy/no-schema keeps
//!    the old behavior.
//! 2. Owned local death (liveness Drop / Stloc overwrite / function frame
//!    exit / root Halt / host-invocation abort / shutdown / reset) releases
//!    the guest owner exactly once through the program's exact local schema;
//!    Pending closes are driven by the scope; synchronous close failures are
//!    recorded in the scope's first-error latch.
//! 3. Move paths never release: `DetachLocal`/`MoveVar` clears the source
//!    slot, `return` moves a resource local out, `TakeOwned` call args move,
//!    resource capture moves — the source frame never re-releases.
//! 4. Nested resource-containing locals release via the exact schema walk
//!    over real runtime `Value`s (Array/Map aggregates); plain `Int`s and
//!    malformed shapes are never released.
//! 5. JIT/AOT: a program with owned locals never traces/executes native; the
//!    interpreter still releases.
//!
//! Only fake generic [`HostResource`] types with close counters are used.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use vm::compiler::{CompileSourceFileOptions, SourceFlavor, TypeSchema};
use vm::resource::{
    CloseProgress, HostResource, ResourceCloseReason, ResourceError, ResourceErrorCode,
    ResourceOwnership, ResourceResult, ResourceTable,
};
use vm::{
    BytecodeBuilder, CallOutcome, CallReturn, HostApiBuilder, HostFunction, HostFunctionRegistry,
    HostFunctionSchema, HostImport, HostParamPassing, HostParamSchema, HostTypeSchema, JitConfig,
    Program, ResourceHandle, ResourceTypeKey, TypeMap, Value, Vm, VmError, VmResult, VmStatus,
    compile_source_with_flavor_and_options,
};

/// A test pending host-operation driver: stays `Pending` until cancelled.
struct PendingOperationDriver;

impl vm::operation::HostOperation for PendingOperationDriver {
    fn poll(&mut self, _cx: &mut Context<'_>) -> Poll<vm::operation::OperationResult<()>> {
        Poll::Pending
    }

    fn cancel(
        &mut self,
        _reason: vm::operation::OperationCancelReason,
    ) -> vm::operation::OperationResult<()> {
        Ok(())
    }
}

// ---- test resources ---------------------------------------------------------

/// Shared close counters for a family of resources.
#[derive(Clone, Default)]
struct CloseCounters {
    begins: Arc<AtomicUsize>,
    reasons: Arc<Mutex<Vec<ResourceCloseReason>>>,
}

impl CloseCounters {
    fn new() -> Self {
        Self {
            begins: Arc::new(AtomicUsize::new(0)),
            reasons: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn began(&self) -> usize {
        self.begins.load(Ordering::SeqCst)
    }

    fn record(&self, reason: ResourceCloseReason) {
        self.begins.fetch_add(1, Ordering::SeqCst);
        self.reasons.lock().unwrap().push(reason);
    }
}

/// Synchronous-close resource sharing one counter set.
struct CountingResource {
    counters: CloseCounters,
}

impl HostResource for CountingResource {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(ResourceTypeKey::new("io.file").expect("valid test key"))
    }

    fn begin_close(&mut self, reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.counters.record(reason);
        Ok(CloseProgress::Ready)
    }
}

/// A resource whose close stays `Pending` until its shared gate is released.
struct GatedResource {
    counters: CloseCounters,
    polls: Arc<AtomicUsize>,
    gate: Arc<AtomicBool>,
}

impl HostResource for GatedResource {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(ResourceTypeKey::new("io.file").expect("valid test key"))
    }

    fn begin_close(&mut self, reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.counters.record(reason);
        Ok(CloseProgress::Pending)
    }

    fn poll_close(&mut self, _cx: &mut Context<'_>) -> Poll<ResourceResult<()>> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        if self.gate.load(Ordering::SeqCst) {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
}

/// A resource whose `begin_close` always fails with a structured error.
struct FailingResource {
    counters: CloseCounters,
}

impl HostResource for FailingResource {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(ResourceTypeKey::new("io.file").expect("valid test key"))
    }

    fn begin_close(&mut self, reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.counters.record(reason);
        Err(ResourceError::new(
            ResourceErrorCode::ResourceCleanupFailed,
            "test::FailingResource",
            format!("deliberate close failure for {reason:?}"),
        ))
    }
}

// ---- host implementations ---------------------------------------------------

/// Dynamic host that pushes a fresh `CountingResource` and returns its raw
/// handle, recording the handle carrier in `handles`.
struct OpenCountingHost {
    counters: CloseCounters,
    handles: Arc<Mutex<Vec<i64>>>,
}

impl HostFunction for OpenCountingHost {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        let resource = CountingResource {
            counters: self.counters.clone(),
        };
        let token = vm.host_context().push_resource(resource).expect("push");
        let raw = token.handle().raw() as i64;
        self.handles.lock().unwrap().push(raw);
        Ok(CallOutcome::Return(CallReturn::One(Value::Int(raw))))
    }
}

/// Dynamic host that pushes a fresh `GatedResource` (shared gate/counters)
/// and returns its raw handle.
struct OpenGatedHost {
    counters: CloseCounters,
    polls: Arc<AtomicUsize>,
    gate: Arc<AtomicBool>,
    handle: Arc<Mutex<Option<i64>>>,
}

impl HostFunction for OpenGatedHost {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        let resource = GatedResource {
            counters: self.counters.clone(),
            polls: Arc::clone(&self.polls),
            gate: Arc::clone(&self.gate),
        };
        let token = vm.host_context().push_resource(resource).expect("push");
        let raw = token.handle().raw() as i64;
        *self.handle.lock().unwrap() = Some(raw);
        Ok(CallOutcome::Return(CallReturn::One(Value::Int(raw))))
    }
}

/// Dynamic host that pushes a fresh `FailingResource` and returns its raw
/// handle.
struct OpenFailingHost {
    counters: CloseCounters,
}

impl HostFunction for OpenFailingHost {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        let resource = FailingResource {
            counters: self.counters.clone(),
        };
        let token = vm.host_context().push_resource(resource).expect("push");
        let raw = token.handle().raw() as i64;
        Ok(CallOutcome::Return(CallReturn::One(Value::Int(raw))))
    }
}

/// Dynamic host that pushes a fresh `CountingResource` and returns
/// `Pending(op_id)` for a real scope-registered operation.
struct PendingOpenHost {
    counters: CloseCounters,
    handle: Arc<Mutex<Option<i64>>>,
}

impl HostFunction for PendingOpenHost {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        let resource = CountingResource {
            counters: self.counters.clone(),
        };
        let token = vm.host_context().push_resource(resource).expect("push");
        let raw = token.handle().raw() as i64;
        *self.handle.lock().unwrap() = Some(raw);
        let op_id = vm
            .host_context()
            .start_operation(vm::operation::OperationSpec::new(PendingOperationDriver))
            .expect("start pending scope operation");
        Ok(CallOutcome::Pending(op_id.raw()))
    }
}

// ---- catalog + compiler helpers ---------------------------------------------

fn io_file_key() -> ResourceTypeKey {
    ResourceTypeKey::new("io.file").expect("valid io.file key")
}

/// Catalog exposing `acme::open(str) -> io.file` (exact `Resource` return),
/// `acme::peek(&io.file)` (Borrow), `acme::take(io.file)` (TakeOwned), and a
/// nested `acme::make_pair(str) -> array<io.file>`.
fn catalog() -> Arc<vm::HostApiCatalog> {
    let file = io_file_key();
    let mut builder = HostApiBuilder::new();
    builder.resource(vm::ResourceTypeSchema::new(file.clone(), "file"));
    builder.function(HostFunctionSchema::with_return(
        "acme::open",
        vec![HostParamSchema::value("path", HostTypeSchema::String)],
        HostTypeSchema::Resource(file.clone()),
    ));
    builder.function(HostFunctionSchema::with_return(
        "acme::peek",
        vec![HostParamSchema::with_passing(
            "f",
            HostTypeSchema::Resource(file.clone()),
            HostParamPassing::Borrow,
        )],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "acme::take",
        vec![HostParamSchema::with_passing(
            "f",
            HostTypeSchema::Resource(file.clone()),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "acme::make_pair",
        vec![HostParamSchema::value("tag", HostTypeSchema::String)],
        HostTypeSchema::Array(Box::new(HostTypeSchema::Resource(file.clone()))),
    ));
    builder.function(HostFunctionSchema::with_return(
        "acme::checkpoint",
        Vec::new(),
        HostTypeSchema::Int,
    ));
    Arc::new(builder.build().expect("catalog must build"))
}

fn compile_catalog_program(source: &str) -> vm::CompiledProgram {
    let source = format!("use acme;\n{source}");
    compile_source_with_flavor_and_options(
        &source,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(catalog()),
    )
    .expect("catalog source should compile")
}

/// Returns the exact `acme::open` import schema from a compiled program.
fn open_import_schema(program: &Program) -> vm::HostImportSchema {
    program
        .imports
        .iter()
        .find(|import| import.name == "acme::open")
        .expect("open import")
        .schema
        .clone()
        .expect("exact schema")
}

/// Returns the exact `acme::peek` import schema from a compiled program.
fn peek_import_schema(program: &Program) -> vm::HostImportSchema {
    program
        .imports
        .iter()
        .find(|import| import.name == "acme::peek")
        .expect("peek import")
        .schema
        .clone()
        .expect("exact schema")
}

/// Registers an exact dynamic `acme::open` host that pushes a fresh
/// `CountingResource` (sharing `counters`) into the caller's scope and
/// returns its raw handle. The returned handle carriers are recorded in
/// `handles`.
fn register_open_dynamic(
    registry: &mut HostFunctionRegistry,
    schema: vm::HostImportSchema,
    counters: CloseCounters,
    handles: Arc<Mutex<Vec<i64>>>,
) {
    registry
        .register_exact("acme::open", 1, schema, move || {
            Box::new(OpenCountingHost {
                counters: counters.clone(),
                handles: Arc::clone(&handles),
            })
        })
        .expect("register open");
}

/// Registers `acme::peek` as an exact VM-aware static no-op returning 0.
///
/// `peek` carries a `Borrow` resource parameter, so it must be registered
/// through a VM-aware wrapper (`register_exact_static`): args-only exact
/// registrations reject any resource passing at registration time.
fn register_peek_noop(registry: &mut HostFunctionRegistry, schema: vm::HostImportSchema) {
    registry
        .register_exact_static("acme::peek", 1, schema, |_vm, _args| {
            Ok(CallOutcome::Return(CallReturn::One(Value::Int(0))))
        })
        .expect("register peek");
}

// ---- helpers -----------------------------------------------------------------

struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

fn noop_waker() -> Waker {
    Waker::from(Arc::new(NoopWake))
}

/// Drives a closing host context to quiescence and returns the outcome.
fn drive_scope(cx: &mut vm::HostContext<'_>) -> vm::execution_scope::ScopeCloseOutcome {
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    loop {
        match cx.poll_close(&mut context) {
            Poll::Pending => continue,
            Poll::Ready(Ok(outcome)) => return outcome,
            Poll::Ready(Err(error)) => panic!("scope close failed: {error}"),
        }
    }
}

fn raw_handle(raw: i64) -> ResourceHandle {
    ResourceHandle::from_raw(raw as u64).expect("valid handle")
}

// ---- 1. exact host return ownership transfer ---------------------------------

/// Exact `Resource` return with a real table handle: the handle's table entry
/// moves HostOwned -> GuestOwned before the value is pushed. The trailing
/// `r;` statement consumes the local as a move (DetachLocal), so the handle
/// survives the run guest-owned (never double-released) and is closed by the
/// scope fallback at shutdown.
#[test]
fn exact_host_return_marks_guest_owned() {
    let compiled = compile_catalog_program("let r = acme::open(\"/tmp/x\"); r;\n");
    let schema = open_import_schema(&compiled.program);
    let counters = CloseCounters::new();
    let handles = Arc::new(Mutex::new(Vec::new()));
    let mut registry = HostFunctionRegistry::new();
    register_open_dynamic(
        &mut registry,
        schema,
        counters.clone(),
        Arc::clone(&handles),
    );

    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    registry.bind_vm_cached(&mut vm).expect("bind");
    let status = vm.run().expect("run");
    assert_eq!(status, VmStatus::Halted);

    let handles = handles.lock().unwrap();
    assert_eq!(handles.len(), 1);
    let handle = raw_handle(handles[0]);
    assert_eq!(
        vm.host_context()
            .execution_scope()
            .resources()
            .ownership(handle),
        Some(ResourceOwnership::GuestOwned),
        "exact host return must transfer ownership to the guest"
    );
    // The statement-level move detached the slot, so the guest-owned handle
    // is NOT released by the source frame; the scope fallback closes it.
    assert_eq!(
        counters.began(),
        0,
        "no release fired from the source frame"
    );
    let mut cx = vm.host_context();
    cx.begin_close(ResourceCloseReason::Requested)
        .expect("begin close");
    let outcome = drive_scope(&mut cx);
    assert_eq!(
        outcome,
        vm::execution_scope::ScopeCloseOutcome::Success,
        "scope fallback closes the moved-out guest-owned handle"
    );
    assert_eq!(counters.began(), 1, "scope fallback closed it exactly once");
}

/// A structurally valid handle from a *foreign* table is rejected by the
/// real exact `Dynamic`/from-stack path. The call has a sentinel below its
/// operand base, so the complete pre-call stack and frame locals must survive
/// the ownership-transfer error.
#[test]
fn exact_host_return_foreign_handle_rejected_stack_frame_unchanged() {
    let imported = compile_catalog_program("let r = acme::open(\"/tmp/x\"); r;\n")
        .program
        .imports
        .into_iter()
        .find(|import| import.name == "acme::open")
        .expect("open import");
    let schema = imported.schema.clone().expect("exact schema");
    let foreign_raw = {
        let mut table = ResourceTable::new().expect("table");
        let token = table.push(CountingResource {
            counters: CloseCounters::new(),
        });
        token.expect("push").handle().raw()
    };

    struct ForeignReturn {
        raw: i64,
    }
    impl HostFunction for ForeignReturn {
        fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
            Ok(CallOutcome::Return(CallReturn::One(Value::Int(self.raw))))
        }
    }

    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact("acme::open", 1, schema, move || {
            Box::new(ForeignReturn {
                raw: foreign_raw as i64,
            })
        })
        .expect("register exact");

    let mut bc = BytecodeBuilder::new();
    bc.ldc(0); // observable sentinel below the host argument
    bc.ldc(1); // host argument
    bc.call(0, 1);
    bc.ret();
    let sentinel = Value::Int(0x05E7_11E3);
    let argument = Value::Int(7);
    let program = Program::with_imports_and_debug(
        vec![sentinel.clone(), argument.clone()],
        bc.finish(),
        vec![HostImport {
            name: imported.name,
            arity: 1,
            return_type: imported.return_type,
            schema: imported.schema,
        }],
        None,
    )
    .with_local_count(1);
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    vm.set_local(0, Value::Int(0x10_CA_11))
        .expect("set frame local");
    let stack_before = vec![sentinel, argument];
    let locals_before = vm.locals().to_vec();
    let call_depth_before = vm.call_depth();

    registry.bind_vm_cached(&mut vm).expect("bind");
    let error = vm
        .run()
        .expect_err("foreign handle return must be rejected");
    assert_eq!(
        error.resource_error_code(),
        Some(ResourceErrorCode::ResourceHandleWrongTable),
        "foreign handle return must be a structured wrong-table rejection, got: {error}"
    );
    assert_eq!(
        vm.stack(),
        stack_before.as_slice(),
        "ownership-transfer failure must restore the complete pre-call operand stack"
    );
    assert_eq!(
        vm.locals(),
        locals_before.as_slice(),
        "ownership-transfer failure must preserve the active frame locals"
    );
    assert_eq!(
        vm.call_depth(),
        call_depth_before,
        "ownership-transfer failure must restore the active call depth"
    );
}

#[test]
fn exact_host_return_rejects_already_guest_owned_handle_as_structured_error() {
    let compiled = compile_catalog_program("acme::open(\"/tmp/x\");\n");
    let schema = open_import_schema(&compiled.program);
    let counters = CloseCounters::new();
    let host_counters = counters.clone();

    struct AlreadyGuestReturn {
        counters: CloseCounters,
    }

    impl HostFunction for AlreadyGuestReturn {
        fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
            let token = vm
                .host_context()
                .push_resource(CountingResource {
                    counters: self.counters.clone(),
                })
                .expect("push");
            let handle = token.handle();
            vm.host_context()
                .mark_resource_guest_owned(handle)
                .expect("pre-mark guest ownership");
            Ok(CallOutcome::Return(CallReturn::One(Value::Int(
                handle.raw() as i64,
            ))))
        }
    }

    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact("acme::open", 1, schema, move || {
            Box::new(AlreadyGuestReturn {
                counters: host_counters.clone(),
            })
        })
        .expect("register exact");
    let mut vm = Vm::try_new(compiled.program).expect("construct VM");
    registry.bind_vm_cached(&mut vm).expect("bind exact host");

    let error = vm
        .run()
        .expect_err("duplicate exact ownership transfer must fail");
    assert_eq!(
        error.resource_error_code(),
        Some(ResourceErrorCode::ResourceNotHostOwned),
        "already-guest exact return must remain a typed duplicate-transfer error: {error}"
    );
    drop(vm);
    assert_eq!(
        counters.began(),
        1,
        "VM teardown must close the pre-marked guest resource exactly once"
    );
}

/// A legacy `schema:None` host return keeps the old behavior: no ownership
/// transfer, no rejection, plain Int flows through.
#[test]
fn legacy_schema_none_return_keeps_old_behavior() {
    let compiled = compile_source_with_flavor_and_options(
        "fn legacy(x);\nlegacy(7);\n",
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default(),
    )
    .expect("compile legacy program");
    let mut registry = HostFunctionRegistry::new();
    registry.register_static_non_yielding_args("legacy", 1, |_| {
        Ok(CallOutcome::Return(CallReturn::One(Value::Int(42))))
    });
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    registry.bind_vm_cached(&mut vm).expect("bind");
    let status = vm.run().expect("legacy Int return must run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

// ---- 2. async Pending completion ownership transfer --------------------------

fn pending_open_program() -> vm::Program {
    let compiled = compile_catalog_program("let r = acme::open(\"/tmp/x\"); r;\n");
    compiled.program
}

/// A Pending exact `Resource` completion marks GuestOwned on the good path.
#[test]
fn exact_async_completion_marks_guest_owned() {
    let program = pending_open_program();
    let schema = open_import_schema(&program);
    let counters = CloseCounters::new();
    let handle = Arc::new(Mutex::new(None::<i64>));
    let handle_for_host = Arc::clone(&handle);
    let counters_for_host = counters.clone();

    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact("acme::open", 1, schema, move || {
            Box::new(PendingOpenHost {
                counters: counters_for_host.clone(),
                handle: Arc::clone(&handle_for_host),
            })
        })
        .expect("register exact");

    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    registry.bind_vm_cached(&mut vm).expect("bind");
    let status = vm.run().expect("first run waits");
    let VmStatus::Waiting(op_id) = status else {
        panic!("expected waiting status, got {status:?}");
    };
    let raw = handle.lock().unwrap().expect("handle captured");
    vm.complete_host_op(op_id, vec![Value::Int(raw)])
        .expect("good completion");
    let resumed = vm.resume().expect("resume halts");
    assert_eq!(resumed, VmStatus::Halted);
    let handle = raw_handle(raw);
    assert_eq!(
        vm.host_context()
            .execution_scope()
            .resources()
            .ownership(handle),
        Some(ResourceOwnership::GuestOwned)
    );
    // The trailing `r;` statement moved the local out; the guest-owned
    // handle is closed by the scope fallback.
    assert_eq!(
        counters.began(),
        0,
        "source frame never releases a moved value"
    );
    let mut cx = vm.host_context();
    cx.begin_close(ResourceCloseReason::Requested)
        .expect("begin close");
    let outcome = drive_scope(&mut cx);
    assert_eq!(outcome, vm::execution_scope::ScopeCloseOutcome::Success);
    assert_eq!(counters.began(), 1, "scope fallback closed it exactly once");
}

/// A Pending completion with a foreign handle is a structured rejection and
/// terminates the waiting op.
#[test]
fn exact_async_completion_foreign_handle_rejected() {
    let program = pending_open_program();
    let schema = open_import_schema(&program);
    let foreign_raw = {
        let mut table = ResourceTable::new().expect("table");
        let token = table.push(CountingResource {
            counters: CloseCounters::new(),
        });
        token.expect("push").handle().raw()
    };

    struct ForeignPending;
    impl HostFunction for ForeignPending {
        fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
            let op_id = vm
                .host_context()
                .start_operation(vm::operation::OperationSpec::new(PendingOperationDriver))
                .expect("start pending scope operation");
            Ok(CallOutcome::Pending(op_id.raw()))
        }
    }

    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact("acme::open", 1, schema, move || Box::new(ForeignPending))
        .expect("register exact");
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    registry.bind_vm_cached(&mut vm).expect("bind");
    let status = vm.run().expect("run waits");
    let VmStatus::Waiting(op_id) = status else {
        panic!("expected waiting status, got {status:?}");
    };
    let error = vm
        .complete_host_op(op_id, vec![Value::Int(foreign_raw as i64)])
        .expect_err("foreign completion must be rejected");
    assert_eq!(
        error.resource_error_code(),
        Some(ResourceErrorCode::ResourceHandleWrongTable),
        "foreign completion must be a structured wrong-table rejection, got: {error}"
    );
    assert_eq!(vm.waiting_host_op_id(), None, "waiting op terminated");
    assert!(vm.stack().is_empty(), "no value pushed");
}

// ---- 3. owned local release: last-use Drop / overwrite / frame exit / Halt ---

/// `let r = open(); peek(&r);` — the liveness Drop after the last use
/// releases the guest owner exactly once with the OwnershipRelease reason.
#[test]
fn local_last_use_drop_releases_once() {
    let compiled = compile_catalog_program("let r = acme::open(\"/tmp/x\");\nacme::peek(&r);\n");
    let open_schema = open_import_schema(&compiled.program);
    let peek_schema = peek_import_schema(&compiled.program);
    let counters = CloseCounters::new();
    let handles = Arc::new(Mutex::new(Vec::new()));
    let mut registry = HostFunctionRegistry::new();
    register_open_dynamic(
        &mut registry,
        open_schema,
        counters.clone(),
        Arc::clone(&handles),
    );
    register_peek_noop(&mut registry, peek_schema);

    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    registry.bind_vm_cached(&mut vm).expect("bind");
    let status = vm.run().expect("run");
    assert_eq!(status, VmStatus::Halted);

    assert_eq!(
        counters.began(),
        1,
        "exactly one close for the last-use Drop"
    );
    let reasons = counters.reasons.lock().unwrap();
    assert_eq!(
        reasons.as_slice(),
        &[ResourceCloseReason::OwnershipRelease],
        "release must close with the ownership-release reason"
    );
    drop(reasons);
    let handles = handles.lock().unwrap();
    let handle = raw_handle(handles[0]);
    // A released (vacant) slot keeps its generation until reuse, so
    // `ownership` reports the reset HostOwned; the decisive check is that no
    // live resource remains and the handle no longer resolves as open.
    assert_eq!(
        vm.host_context().execution_scope().resources().len(),
        0,
        "released resource must leave no live entry"
    );
    assert!(
        vm.host_context()
            .execution_scope()
            .resources()
            .typed::<CountingResource>(handle)
            .is_err(),
        "released handle must no longer validate as open"
    );
}

/// A same-local overwrite (`r = open2()`) releases the old owner exactly once;
/// the second owner is released by the liveness Drop after its last use.
#[test]
fn local_overwrite_releases_old_owner_once() {
    let compiled = compile_catalog_program(
        "let mut r = acme::open(\"/tmp/a\");\nr = acme::open(\"/tmp/b\");\nacme::peek(&r);\n",
    );
    let schema = open_import_schema(&compiled.program);
    let peek_schema = peek_import_schema(&compiled.program);
    let counters = CloseCounters::new();
    let handles = Arc::new(Mutex::new(Vec::new()));
    let mut registry = HostFunctionRegistry::new();
    register_open_dynamic(
        &mut registry,
        schema,
        counters.clone(),
        Arc::clone(&handles),
    );
    register_peek_noop(&mut registry, peek_schema);

    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    registry.bind_vm_cached(&mut vm).expect("bind");
    let status = vm.run().expect("run");
    assert_eq!(status, VmStatus::Halted);

    let handles = handles.lock().unwrap();
    assert_eq!(handles.len(), 2, "two resources created");
    let first = raw_handle(handles[0]);
    let second = raw_handle(handles[1]);
    // Both were released (first by overwrite, second by the liveness Drop).
    assert_eq!(counters.began(), 2, "both closes launched exactly once");
    let reasons = counters.reasons.lock().unwrap();
    assert_eq!(
        reasons.as_slice(),
        &[
            ResourceCloseReason::OwnershipRelease,
            ResourceCloseReason::OwnershipRelease
        ],
        "both releases are ownership releases"
    );
    drop(reasons);
    assert_eq!(
        vm.host_context().execution_scope().resources().len(),
        0,
        "both handles must leave no live entry"
    );
    assert!(
        vm.host_context()
            .execution_scope()
            .resources()
            .typed::<CountingResource>(first)
            .is_err()
    );
    assert!(
        vm.host_context()
            .execution_scope()
            .resources()
            .typed::<CountingResource>(second)
            .is_err()
    );
}

/// A resource local returned from a script function is moved out: the callee
/// frame exit must NOT release it; the caller's liveness Drop releases it
/// exactly once.
#[test]
fn function_frame_exit_releases_callee_locals_not_returned_moves() {
    let compiled = compile_catalog_program(
        r#"
fn make(path) {
    let r = acme::open(path);
    r
}
let a = make("/tmp/a");
acme::peek(&a);
"#,
    );
    let schema = open_import_schema(&compiled.program);
    let peek_schema = peek_import_schema(&compiled.program);
    let counters = CloseCounters::new();
    let handles = Arc::new(Mutex::new(Vec::new()));
    let mut registry = HostFunctionRegistry::new();
    register_open_dynamic(
        &mut registry,
        schema,
        counters.clone(),
        Arc::clone(&handles),
    );
    register_peek_noop(&mut registry, peek_schema);

    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    registry.bind_vm_cached(&mut vm).expect("bind");
    let status = vm.run().expect("run");
    assert_eq!(status, VmStatus::Halted);

    let handles = handles.lock().unwrap();
    assert_eq!(handles.len(), 1, "exactly one resource created");
    assert_eq!(
        counters.began(),
        1,
        "moved-returned resource released exactly once (caller Drop, not callee frame exit)"
    );
    let reasons = counters.reasons.lock().unwrap();
    assert_eq!(
        reasons.as_slice(),
        &[ResourceCloseReason::OwnershipRelease],
        "the release must be an ownership release"
    );
    drop(reasons);
    assert_eq!(vm.host_context().execution_scope().resources().len(), 0);
}

/// A Pending close from a local death is driven to completion by the scope
/// poll machinery; the close is begun exactly once.
#[test]
fn pending_local_release_driven_by_scope_poll() {
    let compiled = compile_catalog_program("let r = acme::open(\"/tmp/x\");\nacme::peek(&r);\n");
    let open_schema = open_import_schema(&compiled.program);
    let peek_schema = peek_import_schema(&compiled.program);
    let counters = CloseCounters::new();
    let polls = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(AtomicBool::new(false));
    let handle = Arc::new(Mutex::new(None::<i64>));
    let counters_for_host = counters.clone();
    let polls_for_host = Arc::clone(&polls);
    let gate_for_host = Arc::clone(&gate);
    let handle_for_host = Arc::clone(&handle);

    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact("acme::open", 1, open_schema, move || {
            Box::new(OpenGatedHost {
                counters: counters_for_host.clone(),
                polls: Arc::clone(&polls_for_host),
                gate: Arc::clone(&gate_for_host),
                handle: Arc::clone(&handle_for_host),
            })
        })
        .expect("register open");
    register_peek_noop(&mut registry, peek_schema);

    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    registry.bind_vm_cached(&mut vm).expect("bind");
    let status = vm.run().expect("run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        counters.began(),
        1,
        "begin_close exactly once at the local death"
    );

    // Release the gate and drive the scope to quiescence.
    gate.store(true, Ordering::SeqCst);
    let mut cx = vm.host_context();
    cx.begin_close(ResourceCloseReason::Requested)
        .expect("begin close");
    let outcome = drive_scope(&mut cx);
    assert_eq!(
        outcome,
        vm::execution_scope::ScopeCloseOutcome::Success,
        "pending close finishes cleanly"
    );
    assert!(polls.load(Ordering::SeqCst) >= 1, "close must be polled");
}

/// A synchronous close failure during a local death is recorded in the
/// scope's first-error latch (never panicked) and surfaces at the terminal
/// scope outcome.
#[test]
fn local_release_close_failure_poisons_scope_terminal() {
    let compiled = compile_catalog_program("let r = acme::open(\"/tmp/x\");\nacme::peek(&r);\n");
    let open_schema = open_import_schema(&compiled.program);
    let peek_schema = peek_import_schema(&compiled.program);
    let counters = CloseCounters::new();
    let mut registry = HostFunctionRegistry::new();
    let open_counters = counters.clone();
    registry
        .register_exact("acme::open", 1, open_schema, move || {
            Box::new(OpenFailingHost {
                counters: open_counters.clone(),
            })
        })
        .expect("register open");
    register_peek_noop(&mut registry, peek_schema);

    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    registry.bind_vm_cached(&mut vm).expect("bind");
    let status = vm.run().expect("run");
    assert_eq!(status, VmStatus::Halted);

    assert_eq!(counters.began(), 1, "begin_close exactly once");
    assert!(
        vm.host_context().execution_scope().first_error().is_some(),
        "close failure must be recorded in the scope error latch"
    );

    let mut cx = vm.host_context();
    cx.begin_close(ResourceCloseReason::Requested)
        .expect("begin close");
    let outcome = drive_scope(&mut cx);
    assert!(
        matches!(
            outcome,
            vm::execution_scope::ScopeCloseOutcome::SuccessWithErrors(_)
        ),
        "terminal outcome must carry the recorded close failure"
    );
}

// ---- 4. move paths never release ---------------------------------------------

/// `Borrow` host args and stack truncation never close; the owner stays alive
/// until the liveness Drop.
#[test]
fn borrow_arg_and_truncate_do_not_release() {
    let compiled = compile_catalog_program(
        "let r = acme::open(\"/tmp/x\");\nlet n = acme::peek(&r);\nacme::peek(&r);\nn;\n",
    );
    let open_schema = open_import_schema(&compiled.program);
    let peek_schema = peek_import_schema(&compiled.program);
    let counters = CloseCounters::new();
    let handles = Arc::new(Mutex::new(Vec::new()));
    let mut registry = HostFunctionRegistry::new();
    register_open_dynamic(
        &mut registry,
        open_schema,
        counters.clone(),
        Arc::clone(&handles),
    );
    register_peek_noop(&mut registry, peek_schema);

    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    registry.bind_vm_cached(&mut vm).expect("bind");
    let status = vm.run().expect("run");
    assert_eq!(status, VmStatus::Halted);

    assert_eq!(counters.began(), 1, "borrows never close; Drop closes once");
    assert_eq!(
        vm.host_context().execution_scope().resources().len(),
        0,
        "borrow + truncate must not release; the liveness Drop closes once"
    );
}

/// `TakeOwned` moves the source local out (DetachLocal + MoveVar): the source
/// frame never releases; the host consumes the handle via `take_owned`.
#[test]
fn take_owned_move_var_never_releases_source() {
    let compiled =
        compile_catalog_program("let r = acme::open(\"/tmp/x\");\nlet n = acme::take(r);\nn;\n");
    let open_schema = open_import_schema(&compiled.program);
    let take_schema = compiled
        .program
        .imports
        .iter()
        .find(|import| import.name == "acme::take")
        .expect("take import")
        .schema
        .clone()
        .expect("exact schema");
    let counters = CloseCounters::new();
    let handles = Arc::new(Mutex::new(Vec::new()));
    let mut registry = HostFunctionRegistry::new();
    register_open_dynamic(
        &mut registry,
        open_schema,
        counters.clone(),
        Arc::clone(&handles),
    );
    registry
        .register_exact("acme::take", 1, take_schema, || Box::new(TakeHost))
        .expect("register take");

    struct TakeHost;
    impl HostFunction for TakeHost {
        fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
            let raw = match args.first() {
                Some(Value::Int(raw)) => *raw,
                _ => return Err(VmError::TypeMismatch("resource handle")),
            };
            let handle = ResourceHandle::from_raw(raw as u64)
                .map_err(|e| VmError::HostError(e.to_string()))?;
            // `take_owned` requires mutable table access; route through the
            // generic host boundary's mut entry point.
            let mut cx = vm.host_context();
            let _taken = cx
                .take_resource::<CountingResource>(handle)
                .map_err(|e| VmError::HostError(e.to_string()))?;
            Ok(CallOutcome::Return(CallReturn::One(Value::Int(7))))
        }
    }

    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    registry.bind_vm_cached(&mut vm).expect("bind");
    let status = vm.run().expect("run");
    assert_eq!(status, VmStatus::Halted);

    assert_eq!(
        counters.began(),
        0,
        "taken resource must never be closed by the VM"
    );
    let handles = handles.lock().unwrap();
    let handle = raw_handle(handles[0]);
    assert_eq!(
        vm.host_context()
            .execution_scope()
            .resources()
            .ownership(handle),
        Some(ResourceOwnership::Taken),
        "taken handle reports Taken"
    );
}

// ---- 5. nested aggregate release ---------------------------------------------

/// A nested `array<io.file>` local (schema-declared) releases every element
/// exactly once via the exact schema walk over the real runtime Array value.
/// The program declares `local 0: Array<Resource>` in its TypeMap; the array
/// value is installed through `set_local` and the root Halt walks it.
#[test]
fn nested_aggregate_array_release_releases_each_element_once() {
    let key = io_file_key();
    let mut bc = BytecodeBuilder::new();
    bc.ret();
    let program = Program::new(Vec::new(), bc.finish())
        .with_type_map(TypeMap {
            local_schemas: vec![Some(TypeSchema::Array(Box::new(TypeSchema::Resource(key))))],
            ..TypeMap::default()
        })
        .with_local_count(1);

    let counters = CloseCounters::new();
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    let mut array_items = Vec::new();
    for _ in 0..2 {
        let resource = CountingResource {
            counters: counters.clone(),
        };
        let token = vm.host_context().push_resource(resource).expect("push");
        let raw = token.handle().raw() as i64;
        // Mark guest-owned so the schema walk releases it at the root Halt.
        vm.host_context()
            .mark_resource_guest_owned(ResourceHandle::from_raw(raw as u64).expect("handle"))
            .expect("mark guest owned");
        array_items.push(Value::Int(raw));
    }
    vm.set_local(0, Value::array(array_items))
        .expect("set local 0");

    let status = vm.run().expect("run");
    assert_eq!(status, VmStatus::Halted);

    assert_eq!(
        counters.began(),
        2,
        "both array elements released via the schema walk"
    );
    let reasons = counters.reasons.lock().unwrap();
    assert_eq!(
        reasons.as_slice(),
        &[
            ResourceCloseReason::OwnershipRelease,
            ResourceCloseReason::OwnershipRelease
        ],
        "both releases are ownership releases"
    );
    drop(reasons);
    assert_eq!(
        vm.host_context().execution_scope().resources().len(),
        0,
        "nested aggregate walk must release every element exactly once"
    );
}

/// A malformed runtime shape is never released and never panics: returning a
/// plain Int for a Resource-return schema is rejected at validation before
/// any release concern.
#[test]
fn malformed_shape_never_released_or_panicked() {
    let compiled = compile_catalog_program("let r = acme::open(\"/tmp/x\");\nacme::peek(&r);\n");
    let open_schema = open_import_schema(&compiled.program);
    let peek_schema = peek_import_schema(&compiled.program);
    let mut registry = HostFunctionRegistry::new();

    struct BadReturn;
    impl HostFunction for BadReturn {
        fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
            Ok(CallOutcome::Return(CallReturn::One(Value::Int(7))))
        }
    }
    registry
        .register_exact("acme::open", 1, open_schema, || Box::new(BadReturn))
        .expect("register open");
    register_peek_noop(&mut registry, peek_schema);
    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    registry.bind_vm_cached(&mut vm).expect("bind");
    let error = vm.run().expect_err("malformed return must be rejected");
    assert!(
        matches!(error, VmError::TypeMismatch("resource handle")),
        "expected structured resource-handle rejection, got: {error}"
    );
}

// ---- 6. JIT/AOT gate ---------------------------------------------------------

fn native_jit_supported() -> bool {
    (cfg!(target_arch = "x86_64")
        && (cfg!(target_os = "windows") || (cfg!(unix) && !cfg!(target_os = "macos"))))
        || (cfg!(target_arch = "aarch64")
            && (cfg!(target_os = "linux") || cfg!(target_os = "macos")))
}

/// A hot loop over an owned local with JIT enabled must never record a native
/// trace: the whole program falls back to the interpreter, and the release
/// still happens exactly once per iteration.
#[test]
fn owned_local_program_never_jit_traces_and_releases_per_iteration() {
    if !native_jit_supported() {
        return;
    }
    let compiled = compile_catalog_program(
        "let mut i = 0;\nwhile i < 3 {\n    let r = acme::open(\"/tmp/x\");\n    acme::peek(&r);\n    i = i + 1;\n}\ni;\n",
    );
    let open_schema = open_import_schema(&compiled.program);
    let peek_schema = peek_import_schema(&compiled.program);
    let counters = CloseCounters::new();
    let handles = Arc::new(Mutex::new(Vec::new()));
    let mut registry = HostFunctionRegistry::new();
    register_open_dynamic(
        &mut registry,
        open_schema,
        counters.clone(),
        Arc::clone(&handles),
    );
    register_peek_noop(&mut registry, peek_schema);

    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    registry.bind_vm_cached(&mut vm).expect("bind");
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 1_024,
    });
    let status = vm.run().expect("loop must run through the interpreter");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(
        counters.began(),
        3,
        "each iteration's owned local must close exactly once"
    );
    assert_eq!(
        vm.jit_native_trace_count(),
        0,
        "owned-local program must never JIT-trace:\n{}",
        vm.dump_jit_info()
    );
}

/// AOT compilation of an owned-local program yields an interpreter-boundary
/// artifact and the run still releases.
#[test]
fn owned_local_program_aot_falls_back_to_interpreter_and_releases() {
    if !native_jit_supported() {
        return;
    }
    let compiled = compile_catalog_program("let r = acme::open(\"/tmp/x\");\nacme::peek(&r);\n");
    let open_schema = open_import_schema(&compiled.program);
    let peek_schema = peek_import_schema(&compiled.program);
    let counters = CloseCounters::new();
    let handles = Arc::new(Mutex::new(Vec::new()));
    let mut registry = HostFunctionRegistry::new();
    register_open_dynamic(
        &mut registry,
        open_schema,
        counters.clone(),
        Arc::clone(&handles),
    );
    register_peek_noop(&mut registry, peek_schema);

    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    registry.bind_vm_cached(&mut vm).expect("bind");
    vm.compile_aot().expect("aot compile should succeed");
    let status = vm.run().expect("run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(counters.began(), 1, "exactly one close");
    assert_eq!(
        vm.aot_exec_count(),
        0,
        "owned-local AOT must never execute native code"
    );
}

// ---- 7. drop-contract flag parity --------------------------------------------

/// The ownership release is completely independent of the drop-contract
/// accounting flag: with the flag enabled or disabled, the release count is
/// identical.
#[test]
fn drop_contract_flag_true_false_release_parity() {
    for enabled in [false, true] {
        let compiled =
            compile_catalog_program("let r = acme::open(\"/tmp/x\");\nacme::peek(&r);\n");
        let open_schema = open_import_schema(&compiled.program);
        let peek_schema = peek_import_schema(&compiled.program);
        let counters = CloseCounters::new();
        let handles = Arc::new(Mutex::new(Vec::new()));
        let mut registry = HostFunctionRegistry::new();
        register_open_dynamic(
            &mut registry,
            open_schema,
            counters.clone(),
            Arc::clone(&handles),
        );
        register_peek_noop(&mut registry, peek_schema);

        let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
        registry.bind_vm_cached(&mut vm).expect("bind");
        vm.set_drop_contract_events_enabled(enabled);
        let status = vm.run().expect("run");
        assert_eq!(status, VmStatus::Halted);
        assert_eq!(
            counters.began(),
            1,
            "release must be flag-independent (enabled={enabled})"
        );
    }
}

// ---- 8. same-local collection rebind (regression) ---------------------------

/// The codegen same-local collection rebind
/// (`files["a"] = r1` lowers to
/// `[ldloc files][push "a"][ldloc/ldc r1][Ldc Null][Stloc S][Call Set 3][Stloc S]`)
/// temporarily nulls slot `S` while the collection Arc is still live on the
/// stack. The VM must NOT release the resource handles inside the still-live
/// collection at that intermediate null-store: the schema walker skips it
/// (same-local rebind guard), and every element is released exactly once when
/// the collection local itself dies.
///
/// A mid-run `acme::checkpoint()` host call sits between the rebind and the
/// collection death; it asserts that *no* close has begun yet while the
/// collection is still live, which a broken null-store release would violate.
#[test]
fn same_local_collection_rebind_never_releases_at_null_store() {
    let compiled = compile_catalog_program(
        "let mut files = {};\nlet r1 = acme::open(\"/a\");\nfiles[\"a\"] = r1;\nlet r2 = acme::open(\"/b\");\nfiles[\"b\"] = r2;\nacme::checkpoint();\nlet n = acme::peek(&files[\"a\"]);\nn;\n",
    );
    let open_schema = open_import_schema(&compiled.program);
    let peek_schema = peek_import_schema(&compiled.program);
    let checkpoint_schema = compiled
        .program
        .imports
        .iter()
        .find(|import| import.name == "acme::checkpoint")
        .expect("checkpoint import")
        .schema
        .clone()
        .expect("exact schema");
    let counters = CloseCounters::new();
    let handles = Arc::new(Mutex::new(Vec::new()));
    let checkpoint_begins = Arc::new(AtomicUsize::new(0));
    let mut registry = HostFunctionRegistry::new();
    register_open_dynamic(
        &mut registry,
        open_schema,
        counters.clone(),
        Arc::clone(&handles),
    );
    register_peek_noop(&mut registry, peek_schema);
    {
        let checkpoint_begins = Arc::clone(&checkpoint_begins);
        let counters = counters.clone();
        registry
            .register_exact("acme::checkpoint", 0, checkpoint_schema, move || {
                let checkpoint_begins = Arc::clone(&checkpoint_begins);
                let counters = counters.clone();
                Box::new(CheckpointHost {
                    checkpoint_begins,
                    counters,
                })
            })
            .expect("register checkpoint");
    }

    struct CheckpointHost {
        checkpoint_begins: Arc<AtomicUsize>,
        counters: CloseCounters,
    }
    impl HostFunction for CheckpointHost {
        fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
            self.checkpoint_begins.fetch_add(1, Ordering::SeqCst);
            assert_eq!(
                self.counters.began(),
                0,
                "no close may begin while the collection is still live"
            );
            Ok(CallOutcome::Return(CallReturn::One(Value::Int(0))))
        }
    }

    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    registry.bind_vm_cached(&mut vm).expect("bind");
    let status = vm.run().expect("run");
    assert_eq!(status, VmStatus::Halted);

    assert_eq!(
        checkpoint_begins.load(Ordering::SeqCst),
        1,
        "checkpoint must have run exactly once"
    );
    // Both elements released exactly once when the collection local dies at
    // the root Halt (no early release at the two intermediate null-stores).
    assert_eq!(
        counters.began(),
        2,
        "same-local rebind must release each element exactly once at the collection death"
    );
    let reasons = counters.reasons.lock().unwrap();
    assert_eq!(
        reasons.as_slice(),
        &[
            ResourceCloseReason::OwnershipRelease,
            ResourceCloseReason::OwnershipRelease
        ],
        "both releases are ownership releases"
    );
    drop(reasons);
    assert_eq!(
        vm.host_context().execution_scope().resources().len(),
        0,
        "released collection leaves no live entry"
    );
}
