//! Focused C1 resource Value/ABI tests.
//!
//! Scope (C1 only — no ownership / dispatch passing):
//! 1. `value_matches_type_schema` accepts only `Value::Int` carriers that
//!    decode as structurally valid resource handles, for callable/script args
//!    and callable returns.
//! 2. Interpreter host returns with an exact `HostImport.schema` whose return
//!    is `TypeSchema::Resource(_)` must be validated *before* the value is
//!    pushed: any non-handle Int is a structured `VmError`. Non-resource exact
//!    returns and `schema:None` keep the legacy coarse behavior. A return
//!    schema that *nests* a resource (`Optional<Resource>`, ...) is an
//!    explicit structured rejection (the current `Value::Int` carrier cannot
//!    represent it).
//! 3. JIT/AOT: a host import whose exact params or return
//!    `contains_resource()` is never native/non-yielding eligible, so its
//!    calls keep exiting to the interpreter (no native scalar/i64 shim).
//!
//! Handles are always produced by a real `ResourceTable::push`; exact schemas
//! and fingerprints come from a real catalog + compiler.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

use vm::compiler::{CompileSourceFileOptions, SourceFlavor, TypeSchema};
use vm::resource::{
    CloseProgress, HostResource, ResourceCloseReason, ResourceOwnership, ResourceResult,
    ResourceTable,
};
use vm::{
    BytecodeBuilder, CallOutcome, CallReturn, HostApiBuilder, HostArgsFunction, HostFunction,
    HostFunctionRegistry, HostFunctionSchema, HostImport, HostOpId, HostParamSchema,
    HostStackFunction, HostTypeSchema, JitConfig, Program, ResourceHandle, ResourceTypeKey,
    ResourceTypeSchema, Value, ValueType, Vm, VmError, VmStatus,
    compile_source_with_flavor_and_options,
};

// ---- tiny test resource ---------------------------------------------------

#[derive(Default)]
struct DummyResource;

impl HostResource for DummyResource {
    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        Ok(CloseProgress::Ready)
    }
}

/// Pushes one real resource into a real table and returns the raw handle token
/// as an `i64` `Value` carrier.
fn real_handle_value() -> i64 {
    let mut table = ResourceTable::new();
    let token = table
        .push(DummyResource)
        .expect("table push should produce a handle");
    let handle: ResourceHandle = token.handle();
    let raw = handle.raw();
    // `raw` must decode back through the structural validator.
    assert_eq!(
        ResourceHandle::from_raw(raw).expect("real handle must be structurally valid"),
        handle
    );
    raw as i64
}

// ---- catalog + compiler helpers -------------------------------------------

fn io_file_key() -> ResourceTypeKey {
    ResourceTypeKey::new("io.file").expect("valid io.file key")
}

/// Catalog exposing `acme::ping(int) -> io.file` and
/// `acme::maybe(int) -> io.file?` (nested resource return).
fn catalog() -> std::sync::Arc<vm::HostApiCatalog> {
    let file = io_file_key();
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(file.clone(), "file"));
    builder.function(HostFunctionSchema::with_return(
        "acme::ping",
        vec![HostParamSchema::value("v", HostTypeSchema::Int)],
        HostTypeSchema::Resource(file.clone()),
    ));
    // A return schema that *nests* a resource cannot be represented by the
    // current `Value::Int` handle carrier; the interpreter must structure-
    // reject any return from such an import (no silent coarse pass).
    builder.function(HostFunctionSchema::with_return(
        "acme::maybe",
        vec![HostParamSchema::value("v", HostTypeSchema::Int)],
        HostTypeSchema::Optional(Box::new(HostTypeSchema::Resource(file))),
    ));
    // A non-resource exact schema (plain Int return): must keep the legacy
    // policy so exact non-resource imports do not regress.
    builder.function(HostFunctionSchema::with_return(
        "acme::ping2",
        vec![HostParamSchema::value("v", HostTypeSchema::Int)],
        HostTypeSchema::Int,
    ));
    std::sync::Arc::new(builder.build().expect("catalog must build"))
}

fn compile_catalog_program(source: &str) -> vm::CompiledProgram {
    // Catalog namespaces must be imported (`use acme;`) before their calls.
    let source = format!("use acme;\n{source}");
    compile_source_with_flavor_and_options(
        &source,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(catalog()),
    )
    .expect("catalog source should compile")
}

/// The compiled `acme::ping` import (schema `Some`, return `Resource`).
fn compiled_ping_import() -> vm::HostImport {
    let compiled = compile_catalog_program("acme::ping(7);\n");
    let import = compiled
        .program
        .imports
        .iter()
        .find(|import| import.name == "acme::ping")
        .expect("ping import")
        .clone();
    assert!(import.schema.is_some(), "ping import must be exact");
    assert_eq!(
        import.schema.as_ref().expect("schema").return_type,
        TypeSchema::Resource(io_file_key())
    );
    import
}

/// Pushes one real resource into the VM's execution scope and returns the raw
/// handle token as an `i64` `Value` carrier. The ownership transfer now
/// requires the handle to belong to the *current* scope's table.
fn vm_scope_handle_value(vm: &mut Vm) -> i64 {
    let token = vm
        .host_context()
        .push_resource(DummyResource)
        .expect("push into active scope");
    let handle: ResourceHandle = token.handle();
    let raw = handle.raw();
    // `raw` must decode back through the structural validator.
    assert_eq!(
        ResourceHandle::from_raw(raw).expect("real handle must be structurally valid"),
        handle
    );
    raw as i64
}

/// Dynamic host that pushes a fresh `DummyResource` into the caller's scope
/// and returns its raw handle (exact `Resource` return).
struct VmScopeHandleHost;

impl HostFunction for VmScopeHandleHost {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> vm::VmResult<CallOutcome> {
        let handle = vm_scope_handle_value(vm);
        Ok(CallOutcome::Return(CallReturn::One(Value::Int(handle))))
    }
}

// ---- per-scenario static-args host fns ------------------------------------
//
// Each scenario gets its own static + fn so parallel tests never race on a
// shared cell. All return `Value::Int`, letting the same exact non-yielding
// static-args path serve both valid-handle and plain-Int returns.

static REJECT_RETURN: AtomicI64 = AtomicI64::new(0);
fn static_reject_return(args: &[Value]) -> vm::VmResult<CallOutcome> {
    let _ = args;
    Ok(CallOutcome::Return(CallReturn::One(Value::Int(
        REJECT_RETURN.load(Ordering::SeqCst),
    ))))
}

fn bind_ping_static_non_yielding_factory(
    program: vm::Program,
    return_cell: &'static AtomicI64,
    returned: i64,
    function: fn(&[Value]) -> vm::VmResult<CallOutcome>,
) -> vm::VmResult<Vm> {
    return_cell.store(returned, Ordering::SeqCst);
    let import = compiled_ping_import();
    let schema = import.schema.clone().expect("exact schema");
    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact_static_non_yielding_args("acme::ping", 1, schema, function)
        .expect("register exact non-yielding");
    let mut vm = Vm::new(program);
    registry.bind_vm_cached(&mut vm)?;
    Ok(vm)
}

fn run_vm(vm: &mut Vm) -> Result<VmStatus, VmError> {
    vm.run()
}

// ---- 1. callable/script arg & return resource schema ----------------------

/// A callable prototype whose schema declares a `Resource` parameter and a
/// `Resource` result, backed by a trivial script function that returns its
/// argument unchanged.
///
/// The root code loads the root callable (`ldloc 0`), pushes the argument
/// constant (index 0), and invokes it via `CallValue` with arity 1.
fn resource_callable_program(argument: i64) -> vm::Program {
    let mut bc = BytecodeBuilder::new();
    // root: callable in local 0, argument constant 0, invoke arity 1.
    bc.ldloc(0);
    bc.ldc(0);
    bc.call_value(1);
    bc.ret();
    let function_entry = bc.position();
    // function body: returns its parameter unchanged.
    bc.ldloc(0);
    bc.ret();
    let function_end = bc.position();

    let key = io_file_key();
    vm::Program::new(vec![Value::Int(argument)], bc.finish())
        .with_local_count(1)
        .with_callable_metadata(
            vec![vm::ScriptFunction {
                entry_ip: function_entry,
                end_ip: function_end,
            }],
            vec![vm::CallablePrototype {
                kind: vm::CallableKind::FunctionItem,
                target: vm::CallableTarget::ScriptFunction(0),
                arity: 1,
                frame_local_count: 1,
                parameter_slots: vec![0],
                capture_source_slots: Vec::new(),
                capture_slots: Vec::new(),
                capture_modes: Vec::new(),
                self_slot: None,
                schema: Some(TypeSchema::Callable {
                    params: vec![TypeSchema::Resource(key.clone())],
                    result: Box::new(TypeSchema::Resource(key)),
                }),
            }],
            vec![
                vm::FunctionRegion {
                    start_ip: 0,
                    end_ip: function_entry,
                    prototype_id: None,
                },
                vm::FunctionRegion {
                    start_ip: function_entry,
                    end_ip: function_end,
                    prototype_id: Some(0),
                },
            ],
            vec![vm::RootCallableBinding {
                local_slot: 0,
                prototype_id: 0,
            }],
        )
}

/// A valid handle carrier enters the callable frame: the argument passes the
/// (structural) resource schema and the result round-trips out.
#[test]
fn callable_arg_structurally_valid_resource_handle_enters_frame() {
    let handle = real_handle_value();
    let program = resource_callable_program(handle);
    let mut vm = Vm::new(program);
    let status = run_vm(&mut vm).expect("valid handle carrier should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(handle)]);
}

/// A plain Int (zero / negative / small positive) is rejected as a callable
/// argument when the parameter schema is `Resource(_)`.
#[test]
fn callable_arg_plain_int_rejected_by_resource_schema() {
    for bad in [0i64, -1, 7, 12345] {
        let program = resource_callable_program(bad);
        let mut vm = Vm::new(program);
        let error = vm
            .run()
            .expect_err("plain int must be rejected by a resource parameter schema");
        assert!(
            matches!(error, VmError::TypeMismatch("callable argument schema")),
            "expected callable argument schema mismatch, got: {error}"
        );
    }
}

// ---- 2. interpreter exact resource host return ----------------------------

/// Exact `Resource` return with a real handle: the value is validated *before*
/// it is pushed and lands on the stack, and ownership transfers to the guest.
#[test]
fn exact_resource_host_return_accepts_valid_handle() {
    let compiled = compile_catalog_program("let r = acme::ping(7); r;\n");
    let import = compiled_ping_import();
    let schema = import.schema.clone().expect("exact schema");
    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact(&import.name, 1, schema, || Box::new(VmScopeHandleHost))
        .expect("register exact dynamic");
    let mut vm = Vm::new(compiled.program);
    registry.bind_vm_cached(&mut vm).expect("bind");
    let status = run_vm(&mut vm).expect("valid handle return should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack().len(), 1, "one handle returned");
    let Value::Int(handle) = vm.stack()[0] else {
        panic!("handle carrier must be an Int");
    };
    assert_eq!(
        vm.host_context()
            .execution_scope()
            .resources()
            .ownership(ResourceHandle::from_raw(handle as u64).expect("valid handle")),
        Some(ResourceOwnership::GuestOwned),
        "exact host return must transfer ownership to the guest"
    );
}

/// Exact `Resource` return whose host produces an arbitrary Int (zero,
/// negative, or a small positive that fails the reserved-space decode) is a
/// structured `VmError` — never silently accepted as a coarse Int.
#[test]
fn exact_resource_host_return_rejects_plain_int() {
    for bad in [0i64, -1, 7, 12345] {
        let compiled = compile_catalog_program("let r = acme::ping(7); r;\n");
        let mut vm = bind_ping_static_non_yielding_factory(
            compiled.program,
            &REJECT_RETURN,
            bad,
            static_reject_return,
        )
        .expect("bind");
        let error = vm
            .run()
            .expect_err("plain int return must be rejected by an exact resource schema");
        assert!(
            matches!(error, VmError::TypeMismatch("resource handle")),
            "expected structured resource-handle mismatch, got: {error}"
        );
    }
}

/// `schema:None` legacy host bindings keep the old coarse Int behavior: a
/// plain Int return is not treated as a resource and executes normally.
#[test]
fn schema_none_legacy_int_return_unaffected() {
    // A plain `fn` declaration (no catalog) produces a `schema:None` import.
    let compiled = compile_source_with_flavor_and_options(
        "fn legacy(x);\nlegacy(7);\n",
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default(),
    )
    .expect("compile legacy program");
    assert!(
        compiled
            .program
            .imports
            .iter()
            .all(|import| import.schema.is_none()),
        "legacy imports must be schema-free"
    );

    // Legacy by-name binding returning an Int, no exact schema.
    let mut registry = HostFunctionRegistry::new();
    registry.register_static_non_yielding_args("legacy", 1, |_| {
        Ok(CallOutcome::Return(CallReturn::One(Value::Int(42))))
    });
    let mut vm = Vm::new(compiled.program);
    registry.bind_vm_cached(&mut vm).expect("legacy bind");
    let status = run_vm(&mut vm).expect("legacy Int return must still run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(42)]);
}

/// A return schema that nests a resource (`Optional<Resource>`) is explicitly
/// structure-rejected: the current ABI cannot represent it, and even a `Null`
/// (which a coarse `Unknown` check would have accepted) must error.
#[test]
fn nested_resource_host_return_explicitly_rejected() {
    let compiled = compile_catalog_program("let m = acme::maybe(7);\n");
    let import = compiled
        .program
        .imports
        .iter()
        .find(|import| import.name == "acme::maybe")
        .expect("maybe import")
        .clone();
    let schema = import.schema.clone().expect("exact schema");
    // The return schema nests a resource (`Optional(Resource)`) rather than
    // being a top-level `Resource(_)` — the exact predicate the runtime
    // policy keys off.
    assert!(
        matches!(
            &schema.return_type,
            TypeSchema::Optional(inner)
                if matches!(inner.as_ref(), TypeSchema::Resource(_))
        ),
        "maybe return must be Optional<Resource>, got: {:?}",
        schema.return_type
    );

    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact_static_non_yielding_args("acme::maybe", 1, schema, |_| {
            Ok(CallOutcome::Return(CallReturn::One(Value::Null)))
        })
        .expect("register nested-resource exact host");
    let mut vm = Vm::new(compiled.program);
    registry.bind_vm_cached(&mut vm).expect("bind");

    let error = vm
        .run()
        .expect_err("nested-resource return must be a structured rejection");
    assert!(
        matches!(error, VmError::TypeMismatch(_)),
        "expected structured type mismatch, got: {error}"
    );
}

// ---- 3. JIT / AOT eligibility guard ---------------------------------------

fn native_jit_supported() -> bool {
    (cfg!(target_arch = "x86_64")
        && (cfg!(target_os = "windows") || (cfg!(unix) && !cfg!(target_os = "macos"))))
        || (cfg!(target_arch = "aarch64")
            && (cfg!(target_os = "linux") || cfg!(target_os = "macos")))
}

/// A non-resource non-yielding host import stays native-eligible: recorded
/// traces lower a native `host_call` op (no resource-bearing schema).
#[test]
fn nonresource_nonyielding_import_remains_native_eligible() {
    if !native_jit_supported() {
        return;
    }
    // Loop program calling import #0 (a plain Int-returning non-yielding host).
    // The host's return is discarded (`pop`) so the loop counter in local 0 is
    // independent of the RNG-free host result and the loop terminates.
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.stloc(0);
    let root = bc.position();
    bc.ldloc(0);
    bc.call(0, 1);
    bc.pop();
    bc.ldloc(0);
    bc.ldc(1);
    bc.add();
    bc.stloc(0);
    bc.ldloc(0);
    // constant index 2 == value 4 (constants: [0, 1, 4]).
    bc.ldc(2);
    bc.ceq();
    bc.brfalse(root);
    bc.ret();

    let import = HostImport {
        name: "acme::int_host".into(),
        arity: 1,
        return_type: ValueType::Int,
        schema: None, // legacy, non-resource
    };
    let program = Program::with_imports_and_debug(
        vec![Value::Int(0), Value::Int(1), Value::Int(4)],
        bc.finish(),
        vec![import],
        None,
    )
    .with_local_count(1);

    let mut registry = HostFunctionRegistry::new();
    registry.register_static_non_yielding_args("acme::int_host", 1, |args| {
        Ok(CallOutcome::Return(CallReturn::One(args[0].clone())))
    });
    let mut vm = Vm::new(program);
    registry.bind_vm_cached(&mut vm).expect("bind");
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 1_024,
    });

    let status = run_vm(&mut vm).expect("loop should run");
    assert_eq!(status, VmStatus::Halted);
    assert!(
        vm.jit_native_trace_count() > 0,
        "non-resource non-yielding import must stay native eligible:\n{}",
        vm.dump_jit_info()
    );
}

/// A resource-bearing exact host import is never marked non-yielding native
/// eligible: the loop still exits to the interpreter and executes correctly,
/// and no native trace lowers a `host_call` for it.
#[test]
fn resource_import_is_not_native_eligible_and_trace_exits() {
    // Loop program calling import #0 (an exact `Resource`-returning
    // non-yielding host). The host's return is discarded (`pop`); only the
    // interpreter path validates it, so the loop terminates on its own counter.
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.stloc(0);
    let root = bc.position();
    bc.ldloc(0);
    bc.call(0, 1);
    bc.pop();
    bc.ldloc(0);
    bc.ldc(1);
    bc.add();
    bc.stloc(0);
    bc.ldloc(0);
    // constant index 2 == value 4 (constants: [0, 1, 4]).
    bc.ldc(2);
    bc.ceq();
    bc.brfalse(root);
    bc.ret();

    let imported = compiled_ping_import();
    let schema = imported.schema.clone().expect("exact schema");
    let program = Program::with_imports_and_debug(
        vec![Value::Int(0), Value::Int(1), Value::Int(4)],
        bc.finish(),
        vec![HostImport {
            name: imported.name.clone(),
            arity: 1,
            return_type: imported.return_type,
            schema: Some(schema.clone()),
        }],
        None,
    )
    .with_local_count(1);

    let mut registry = HostFunctionRegistry::new();
    let import_name = imported.name.clone();
    registry
        .register_exact(&import_name, 1, schema, || Box::new(VmScopeHandleHost))
        .expect("register resource exact dynamic");
    let mut vm = Vm::new(program);
    registry.bind_vm_cached(&mut vm).expect("bind");
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 1_024,
    });

    let status = run_vm(&mut vm).expect("resource loop must execute correctly");
    assert_eq!(status, VmStatus::Halted);

    if native_jit_supported() {
        // The eligibility guard must prevent a native non-yielding lowering:
        // either no native trace was recorded, or none of them inline a
        // `host_call` (the trace exits to the interpreter at the call).
        let snapshot = vm.jit_snapshot();
        let host_call_traces = snapshot
            .traces
            .iter()
            .filter(|trace| trace.op_names().iter().any(|op| op == "host_call"))
            .collect::<Vec<_>>();
        assert!(
            host_call_traces.is_empty(),
            "resource import must not be lowered as a native host_call:\n{}",
            vm.dump_jit_info()
        );
    }
}

// ---- 4. async Pending completion (F1) --------------------------------------
//
// A bound host function that returns `CallOutcome::Pending` leaves the VM
// waiting on a `WaitingHostOp`. When the bridge later delivers values (via
// `complete_host_op` / the polled future) they must be validated against the
// exact-return policy captured at the *actual call site* before any stack or
// frame mutation. A good handle is pushed; a plain Int / nested return is a
// structured rejection that also terminates the waiting op (no re-poll can
// deliver the bad values again).

/// An args-only host op that reports `Pending` once.
struct PendingArgsHost {
    op_id: HostOpId,
    call_count: Arc<AtomicUsize>,
}

impl HostArgsFunction for PendingArgsHost {
    fn call(&mut self, _args: &[Value]) -> vm::VmResult<CallOutcome> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        Ok(CallOutcome::Pending(self.op_id))
    }
}

/// A stack-borrowed host op that reports `Pending` once.
struct PendingStackHost {
    op_id: HostOpId,
    call_count: Arc<AtomicUsize>,
}

impl HostStackFunction for PendingStackHost {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> vm::VmResult<CallOutcome> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        Ok(CallOutcome::Pending(self.op_id))
    }
}

/// A VM-aware host op that reports `Pending` once.
struct PendingDynamicHost {
    op_id: HostOpId,
    call_count: Arc<AtomicUsize>,
}

impl HostFunction for PendingDynamicHost {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> vm::VmResult<CallOutcome> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        Ok(CallOutcome::Pending(self.op_id))
    }
}

/// Bytecode program: `ldc 0` (argument), call import 0 arity 1, `ret`.
/// The single import is `acme::ping` with an exact `TypeSchema::Resource(_)`
/// return — the same resource ABI target as the interpreter tests above.
fn pending_resource_call_program() -> vm::Program {
    let imported = compiled_ping_import();
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.call(0, 1);
    bc.ret();
    Program::with_imports_and_debug(
        vec![Value::Int(7)],
        bc.finish(),
        vec![HostImport {
            name: imported.name.clone(),
            arity: 1,
            return_type: imported.return_type,
            schema: imported.schema.clone(),
        }],
        None,
    )
}

fn bind_ping_exact_args(
    program: vm::Program,
    factory: impl Fn() -> Box<dyn HostArgsFunction> + Send + Sync + 'static,
) -> vm::VmResult<Vm> {
    let import = compiled_ping_import();
    let schema = import.schema.clone().expect("exact schema");
    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact_args(&import.name, 1, schema, factory)
        .expect("register exact args");
    let mut vm = Vm::new(program);
    registry.bind_vm_cached(&mut vm)?;
    Ok(vm)
}

/// Exact `Resource` return, args-dynamic host that yields `Pending`: a later
/// legitimate real table handle completing the op is validated and pushed, and
/// the resumed frame halts with the handle on the stack. The host is not
/// re-entered.
#[test]
fn exact_resource_args_dynamic_pending_completion_accepts_valid_handle() {
    let calls = Arc::new(AtomicUsize::new(0));
    let bound_calls = Arc::clone(&calls);
    let op_id = 0xC0_DE_00_01;
    let mut vm = bind_ping_exact_args(pending_resource_call_program(), move || {
        Box::new(PendingArgsHost {
            op_id,
            call_count: Arc::clone(&bound_calls),
        })
    })
    .expect("bind");

    let status = vm.run().expect("first run should wait on host op");
    assert_eq!(status, VmStatus::Waiting(op_id));
    assert_eq!(calls.load(Ordering::SeqCst), 1, "host op should run once");
    assert!(
        vm.stack().is_empty(),
        "pending args-only call consumes args"
    );

    let handle = vm_scope_handle_value(&mut vm);
    vm.complete_host_op(op_id, vec![Value::Int(handle)])
        .expect("valid handle completion should succeed");
    assert_eq!(
        vm.waiting_host_op_id(),
        None,
        "completion clears the waiting op"
    );

    let resumed = vm.resume().expect("resume should halt after completion");
    assert_eq!(resumed, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::Int(handle)],
        "validated handle must be pushed"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "resume must not re-enter the host function"
    );
}

/// Exact `Resource` return, args-dynamic host that yields `Pending`: a plain
/// Int completing the op is a structured rejection. The waiting op is
/// terminated (a re-completion cannot deliver the bad value) and no value is
/// ever pushed onto the stack.
#[test]
fn exact_resource_args_dynamic_pending_completion_rejects_plain_int() {
    // Each bad value needs a fresh VM: a rejected completion terminates the
    // waiting op, so no second completion is possible on the same VM.
    for bad in [0i64, -1, 7, 12345] {
        let calls = Arc::new(AtomicUsize::new(0));
        let bound_calls = Arc::clone(&calls);
        let op_id = 0xC0_DE_00_02;
        let mut vm = bind_ping_exact_args(pending_resource_call_program(), move || {
            Box::new(PendingArgsHost {
                op_id,
                call_count: Arc::clone(&bound_calls),
            })
        })
        .expect("bind");

        let status = vm.run().expect("first run should wait on host op");
        assert_eq!(status, VmStatus::Waiting(op_id));
        assert!(
            vm.stack().is_empty(),
            "pending args-only call consumes args"
        );

        let error = vm
            .complete_host_op(op_id, vec![Value::Int(bad)])
            .expect_err("plain int completion must be rejected");
        assert!(
            matches!(error, VmError::TypeMismatch("resource handle")),
            "expected structured resource-handle mismatch, got: {error}"
        );
        assert_eq!(
            vm.waiting_host_op_id(),
            None,
            "a bad completion must terminate the waiting op"
        );
        assert!(
            vm.stack().is_empty(),
            "no value may be pushed after a rejected completion"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "host op must run only once"
        );
    }

    // A follow-up completion (even with a good handle) cannot resurrect the
    // value: the terminated waiting op refuses a second delivery and nothing
    // is pushed.
    let calls = Arc::new(AtomicUsize::new(0));
    let bound_calls = Arc::clone(&calls);
    let op_id = 0xC0_DE_00_02;
    let mut vm = bind_ping_exact_args(pending_resource_call_program(), move || {
        Box::new(PendingArgsHost {
            op_id,
            call_count: Arc::clone(&bound_calls),
        })
    })
    .expect("bind");
    let status = vm.run().expect("first run should wait on host op");
    assert_eq!(status, VmStatus::Waiting(op_id));
    let error = vm
        .complete_host_op(op_id, vec![Value::Int(7)])
        .expect_err("plain int completion must be rejected");
    assert!(
        matches!(error, VmError::TypeMismatch("resource handle")),
        "expected structured resource-handle mismatch, got: {error}"
    );
    assert_eq!(
        vm.waiting_host_op_id(),
        None,
        "waiting op must be terminated"
    );
    let again = vm
        .complete_host_op(op_id, vec![Value::Int(real_handle_value())])
        .expect_err("terminated waiting op must refuse a follow-up completion");
    assert!(
        matches!(again, VmError::HostError(_)),
        "expected a host error for completing an op the vm no longer waits on, got: {again}"
    );
    assert!(vm.stack().is_empty(), "the follow-up must not push either");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "host op must run only once"
    );
}

/// A return schema that nests a resource (`Optional<Resource>`) is rejected
/// even through the async Pending path: completing the op with any value is a
/// structured rejection and terminates the waiting op.
#[test]
fn nested_resource_args_dynamic_pending_completion_explicitly_rejected() {
    let compiled = compile_catalog_program("acme::maybe(7);\n");
    let import = compiled
        .program
        .imports
        .iter()
        .find(|import| import.name == "acme::maybe")
        .expect("maybe import")
        .clone();
    let schema = import.schema.clone().expect("exact schema");
    assert!(
        matches!(
            &schema.return_type,
            TypeSchema::Optional(inner) if matches!(inner.as_ref(), TypeSchema::Resource(_))
        ),
        "maybe return must nest a resource, got: {:?}",
        schema.return_type
    );

    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.call(0, 1);
    bc.ret();
    let program = Program::with_imports_and_debug(
        vec![Value::Int(7)],
        bc.finish(),
        vec![HostImport {
            name: import.name.clone(),
            arity: 1,
            return_type: import.return_type,
            schema: Some(schema.clone()),
        }],
        None,
    );

    // Each value needs a fresh VM: a rejected nested-resource completion
    // terminates the waiting op, so no second completion is possible on the
    // same VM.
    for value in [Value::Null, Value::Int(real_handle_value())] {
        let calls = Arc::new(AtomicUsize::new(0));
        let bound_calls = Arc::clone(&calls);
        let op_id = 0xC0_DE_00_03;
        let mut registry = HostFunctionRegistry::new();
        registry
            .register_exact_args(&import.name, 1, schema.clone(), move || {
                Box::new(PendingArgsHost {
                    op_id,
                    call_count: Arc::clone(&bound_calls),
                })
            })
            .expect("register nested-resource exact args");
        let mut vm = Vm::new(program.clone());
        registry.bind_vm_cached(&mut vm).expect("bind");

        let status = vm.run().expect("first run should wait on host op");
        assert_eq!(status, VmStatus::Waiting(op_id));
        assert!(vm.stack().is_empty());

        // Even a `Null`/valid handle (which a coarse check would have
        // accepted) must be structure-rejected through the async completion
        // path.
        let error = vm
            .complete_host_op(op_id, CallReturn::one(value))
            .expect_err("nested-resource completion must be a structured rejection");
        assert!(
            matches!(error, VmError::TypeMismatch(_)),
            "expected structured type mismatch, got: {error}"
        );
        assert_eq!(
            vm.waiting_host_op_id(),
            None,
            "rejected completion must terminate the waiting op"
        );
        assert!(vm.stack().is_empty(), "no value may be pushed");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

/// `schema:None` legacy bindings keep the old coarse behavior through the
/// async Pending path: a plain Int completing the op is pushed normally.
#[test]
fn schema_none_args_dynamic_pending_completion_keeps_legacy_behavior() {
    let calls = Arc::new(AtomicUsize::new(0));
    let call_count = Arc::clone(&calls);
    let op_id = 0xC0_DE_00_04;
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.call(0, 1);
    bc.ret();
    let mut vm = Vm::new(Program::new(vec![Value::Int(4)], bc.finish()));
    vm.register_args_function(Box::new(PendingArgsHost { op_id, call_count }));

    let status = vm.run().expect("first run should wait on host op");
    assert_eq!(status, VmStatus::Waiting(op_id));
    assert!(vm.stack().is_empty());

    vm.complete_host_op(op_id, vec![Value::Int(42)])
        .expect("legacy schema-free completion must stay accepted");
    let resumed = vm.resume().expect("resume should halt");
    assert_eq!(resumed, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::Int(42)],
        "legacy completion must push the coarse value"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// A non-resource exact schema (`schema:Some`, plain Int return) keeps the
/// legacy policy through the async Pending path — no regression for exact
/// non-resource imports.
#[test]
fn nonresource_exact_args_dynamic_pending_completion_unaffected() {
    let compiled = compile_catalog_program("acme::ping2(7);\n");
    let import = compiled
        .program
        .imports
        .iter()
        .find(|import| import.name == "acme::ping2")
        .expect("ping2 import")
        .clone();
    assert!(
        import.schema.is_some()
            && matches!(
                import.schema.as_ref().expect("schema").return_type,
                TypeSchema::Int
            ),
        "ping2 must be exact but non-resource, got: {:?}",
        import.schema.as_ref().map(|schema| &schema.return_type)
    );
    let schema = import.schema.clone().expect("exact schema");

    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.call(0, 1);
    bc.ret();
    let program = Program::with_imports_and_debug(
        vec![Value::Int(7)],
        bc.finish(),
        vec![HostImport {
            name: import.name.clone(),
            arity: 1,
            return_type: import.return_type,
            schema: Some(schema.clone()),
        }],
        None,
    );

    let calls = Arc::new(AtomicUsize::new(0));
    let bound_calls = Arc::clone(&calls);
    let op_id = 0xC0_DE_00_05;
    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact_args(&import.name, 1, schema, move || {
            Box::new(PendingArgsHost {
                op_id,
                call_count: Arc::clone(&bound_calls),
            })
        })
        .expect("register exact non-resource args");
    let mut vm = Vm::new(program);
    registry.bind_vm_cached(&mut vm).expect("bind");

    let status = vm.run().expect("first run should wait on host op");
    assert_eq!(status, VmStatus::Waiting(op_id));

    vm.complete_host_op(op_id, vec![Value::Int(77)])
        .expect("non-resource exact completion must stay accepted");
    let resumed = vm.resume().expect("resume should halt");
    assert_eq!(resumed, VmStatus::Halted);
    assert_eq!(
        vm.stack(),
        &[Value::Int(77)],
        "non-resource exact completion must push the value"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// The VM-aware `Dynamic` host path (`execute_bound_host_function_from_stack`)
/// must also carry the call-site exact-return policy into the Pending state: a
/// real handle completing the op is validated and pushed.
#[test]
fn exact_resource_dynamic_pending_completion_accepts_valid_handle() {
    let import = compiled_ping_import();
    let schema = import.schema.clone().expect("exact schema");
    let calls = Arc::new(AtomicUsize::new(0));
    let bound_calls = Arc::clone(&calls);
    let op_id = 0xC0_DE_00_06;
    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact(&import.name, 1, schema, move || {
            Box::new(PendingDynamicHost {
                op_id,
                call_count: Arc::clone(&bound_calls),
            })
        })
        .expect("register exact dynamic");
    let mut vm = Vm::new(pending_resource_call_program());
    registry.bind_vm_cached(&mut vm).expect("bind");

    let status = vm.run().expect("first run should wait on host op");
    assert_eq!(status, VmStatus::Waiting(op_id));
    assert!(vm.stack().is_empty());

    let handle = vm_scope_handle_value(&mut vm);
    vm.complete_host_op(op_id, vec![Value::Int(handle)])
        .expect("valid handle completion should succeed");
    let resumed = vm.resume().expect("resume should halt");
    assert_eq!(resumed, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(handle)]);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// Bad Int completing the `Dynamic` Pending path is rejected before the value
/// reaches the stack, and the waiting op is terminated.
#[test]
fn exact_resource_dynamic_pending_completion_rejects_plain_int() {
    let import = compiled_ping_import();
    let schema = import.schema.clone().expect("exact schema");
    let op_id = 0xC0_DE_00_07;
    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact(&import.name, 1, schema, move || {
            Box::new(PendingDynamicHost {
                op_id,
                call_count: Arc::new(AtomicUsize::new(0)),
            })
        })
        .expect("register exact dynamic");
    let mut vm = Vm::new(pending_resource_call_program());
    registry.bind_vm_cached(&mut vm).expect("bind");

    let status = vm.run().expect("first run should wait on host op");
    assert_eq!(status, VmStatus::Waiting(op_id));
    assert!(vm.stack().is_empty());

    let error = vm
        .complete_host_op(op_id, vec![Value::Int(7)])
        .expect_err("plain int completion must be rejected");
    assert!(
        matches!(error, VmError::TypeMismatch("resource handle")),
        "expected structured resource-handle mismatch, got: {error}"
    );
    assert_eq!(
        vm.waiting_host_op_id(),
        None,
        "waiting op must be terminated"
    );
    assert!(vm.stack().is_empty(), "no value may be pushed");
}

/// The borrowed-stack `StackDynamic` host path must also carry the call-site
/// exact-return policy into the Pending state: a real handle completing the op
/// is validated and pushed; a plain Int is a structured rejection that
/// terminates the waiting op.
#[test]
fn exact_resource_stack_dynamic_pending_completion_validates_handle() {
    let import = compiled_ping_import();
    let schema = import.schema.clone().expect("exact schema");
    let calls = Arc::new(AtomicUsize::new(0));
    let bound_calls = Arc::clone(&calls);
    let op_id = 0xC0_DE_00_08;
    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact_stack(&import.name, 1, schema, move || {
            Box::new(PendingStackHost {
                op_id,
                call_count: Arc::clone(&bound_calls),
            })
        })
        .expect("register exact stack");
    let mut vm = Vm::new(pending_resource_call_program());
    registry.bind_vm_cached(&mut vm).expect("bind");

    let status = vm.run().expect("first run should wait on host op");
    assert_eq!(status, VmStatus::Waiting(op_id));
    assert!(vm.stack().is_empty());

    // A plain Int completing the op is rejected and terminates the waiting op.
    let error = vm
        .complete_host_op(op_id, vec![Value::Int(7)])
        .expect_err("plain int completion must be rejected");
    assert!(
        matches!(error, VmError::TypeMismatch("resource handle")),
        "expected structured resource-handle mismatch, got: {error}"
    );
    assert_eq!(
        vm.waiting_host_op_id(),
        None,
        "rejected completion must terminate the waiting op"
    );
    assert!(vm.stack().is_empty(), "no value may be pushed");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // A fresh call with a real table handle is validated and pushed.
    let op_id2 = 0xC0_DE_00_09;
    let calls2 = Arc::new(AtomicUsize::new(0));
    let bound_calls2 = Arc::clone(&calls2);
    let import = compiled_ping_import();
    let schema = import.schema.clone().expect("exact schema");
    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact_stack(&import.name, 1, schema, move || {
            Box::new(PendingStackHost {
                op_id: op_id2,
                call_count: Arc::clone(&bound_calls2),
            })
        })
        .expect("register exact stack");
    let mut vm = Vm::new(pending_resource_call_program());
    registry.bind_vm_cached(&mut vm).expect("bind");

    let status = vm.run().expect("first run should wait on host op");
    assert_eq!(status, VmStatus::Waiting(op_id2));
    let handle = vm_scope_handle_value(&mut vm);
    vm.complete_host_op(op_id2, vec![Value::Int(handle)])
        .expect("valid handle completion should succeed");
    let resumed = vm.resume().expect("resume should halt");
    assert_eq!(resumed, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(handle)]);
    assert_eq!(calls2.load(Ordering::SeqCst), 1);
}

// ---- 5. immediate-return ordering (F2) -------------------------------------
//
// On an immediate bad resource return the validation must fail BEFORE the
// call operands are truncated from the stack, so the stack keeps its pre-call
// snapshot and no half-truncated state is observable.

struct ImmediateArgsHost {
    returned: i64,
}

impl HostArgsFunction for ImmediateArgsHost {
    fn call(&mut self, _args: &[Value]) -> vm::VmResult<CallOutcome> {
        Ok(CallOutcome::Return(CallReturn::One(Value::Int(
            self.returned,
        ))))
    }
}

struct ImmediateStackHost {
    returned: i64,
}

impl HostStackFunction for ImmediateStackHost {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> vm::VmResult<CallOutcome> {
        Ok(CallOutcome::Return(CallReturn::One(Value::Int(
            self.returned,
        ))))
    }
}

struct ImmediateDynamicHost {
    returned: i64,
}

impl HostFunction for ImmediateDynamicHost {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> vm::VmResult<CallOutcome> {
        Ok(CallOutcome::Return(CallReturn::One(Value::Int(
            self.returned,
        ))))
    }
}

/// ArgsDynamic immediate bad resource return: the structured rejection must
/// leave the call operands on the stack (validation precedes truncation).
#[test]
fn exact_resource_args_dynamic_immediate_bad_return_keeps_stack_snapshot() {
    let mut vm = bind_ping_exact_args(pending_resource_call_program(), || {
        Box::new(ImmediateArgsHost { returned: 7 })
    })
    .expect("bind");

    let error = vm
        .run()
        .expect_err("plain int immediate return must be rejected");
    assert!(
        matches!(error, VmError::TypeMismatch("resource handle")),
        "expected structured resource-handle mismatch, got: {error}"
    );
    assert_eq!(
        vm.stack(),
        &[Value::Int(7)],
        "call args must survive an immediate bad resource return (no truncate-before-validate)"
    );
}

/// ArgsDynamic immediate valid handle return: validated and pushed.
#[test]
fn exact_resource_args_dynamic_immediate_valid_handle_pushed() {
    let import = compiled_ping_import();
    let schema = import.schema.clone().expect("exact schema");
    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact(&import.name, 1, schema, || Box::new(VmScopeHandleHost))
        .expect("register exact dynamic");
    let mut vm = Vm::new(pending_resource_call_program());
    registry.bind_vm_cached(&mut vm).expect("bind");

    let status = vm.run().expect("valid handle immediate return should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack().len(), 1, "one handle returned");
}

/// StackDynamic immediate bad resource return: same no-truncate-before-validate
/// guarantee for the borrowed-stack host path.
#[test]
fn exact_resource_stack_dynamic_immediate_bad_return_keeps_stack_snapshot() {
    let import = compiled_ping_import();
    let schema = import.schema.clone().expect("exact schema");
    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact_stack(&import.name, 1, schema, || {
            Box::new(ImmediateStackHost { returned: 7 })
        })
        .expect("register exact stack");
    let mut vm = Vm::new(pending_resource_call_program());
    registry.bind_vm_cached(&mut vm).expect("bind");

    let error = vm
        .run()
        .expect_err("plain int immediate stack return must be rejected");
    assert!(
        matches!(error, VmError::TypeMismatch("resource handle")),
        "expected structured resource-handle mismatch, got: {error}"
    );
    assert_eq!(
        vm.stack(),
        &[Value::Int(7)],
        "stack-args must survive an immediate bad resource return"
    );
}

/// StackDynamic immediate valid handle return: validated and pushed.
#[test]
fn exact_resource_stack_dynamic_immediate_valid_handle_pushed() {
    let import = compiled_ping_import();
    let schema = import.schema.clone().expect("exact schema");
    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact(&import.name, 1, schema, || Box::new(VmScopeHandleHost))
        .expect("register exact dynamic");
    let mut vm = Vm::new(pending_resource_call_program());
    registry.bind_vm_cached(&mut vm).expect("bind");

    let status = vm.run().expect("valid handle stack return should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack().len(), 1, "one handle returned");
}

/// Dynamic (from-stack) immediate bad resource return: validation failure
/// restores the pre-call snapshot instead of leaving a truncated/empty stack.
#[test]
fn exact_resource_dynamic_immediate_bad_return_keeps_stack_snapshot() {
    let import = compiled_ping_import();
    let schema = import.schema.clone().expect("exact schema");
    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact(&import.name, 1, schema, || {
            Box::new(ImmediateDynamicHost { returned: 7 })
        })
        .expect("register exact dynamic");
    let mut vm = Vm::new(pending_resource_call_program());
    registry.bind_vm_cached(&mut vm).expect("bind");

    let error = vm
        .run()
        .expect_err("plain int immediate dynamic return must be rejected");
    assert!(
        matches!(error, VmError::TypeMismatch("resource handle")),
        "expected structured resource-handle mismatch, got: {error}"
    );
    assert_eq!(
        vm.stack(),
        &[Value::Int(7)],
        "pre-call snapshot must be restored when the dynamic return fails validation"
    );
}
