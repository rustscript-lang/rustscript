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

use std::sync::atomic::{AtomicI64, Ordering};

use vm::compiler::{CompileSourceFileOptions, SourceFlavor, TypeSchema};
use vm::resource::{
    CloseProgress, HostResource, ResourceCloseReason, ResourceResult, ResourceTable,
};
use vm::{
    BytecodeBuilder, CallOutcome, CallReturn, HostApiBuilder, HostFunctionRegistry,
    HostFunctionSchema, HostImport, HostParamSchema, HostTypeSchema, JitConfig, Program,
    ResourceHandle, ResourceTypeKey, ResourceTypeSchema, Value, ValueType, Vm, VmError, VmStatus,
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

// ---- per-scenario static-args host fns ------------------------------------
//
// Each scenario gets its own static + fn so parallel tests never race on a
// shared cell. All return `Value::Int`, letting the same exact non-yielding
// static-args path serve both valid-handle and plain-Int returns.

static ACCEPT_RETURN: AtomicI64 = AtomicI64::new(i64::MIN);
fn static_accept_return(args: &[Value]) -> vm::VmResult<CallOutcome> {
    let _ = args;
    Ok(CallOutcome::Return(CallReturn::One(Value::Int(
        ACCEPT_RETURN.load(Ordering::SeqCst),
    ))))
}

static REJECT_RETURN: AtomicI64 = AtomicI64::new(0);
fn static_reject_return(args: &[Value]) -> vm::VmResult<CallOutcome> {
    let _ = args;
    Ok(CallOutcome::Return(CallReturn::One(Value::Int(
        REJECT_RETURN.load(Ordering::SeqCst),
    ))))
}

static LOOP_RETURN: AtomicI64 = AtomicI64::new(i64::MIN);
fn static_loop_return(args: &[Value]) -> vm::VmResult<CallOutcome> {
    let _ = args;
    Ok(CallOutcome::Return(CallReturn::One(Value::Int(
        LOOP_RETURN.load(Ordering::SeqCst),
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

/// Exact `Resource` return with a structurally valid handle carrier: the
/// value is validated *before* it is pushed and lands on the stack.
#[test]
fn exact_resource_host_return_accepts_valid_handle() {
    let compiled = compile_catalog_program("let r = acme::ping(7); r;\n");
    let handle = real_handle_value();
    let mut vm = bind_ping_static_non_yielding_factory(
        compiled.program,
        &ACCEPT_RETURN,
        handle,
        static_accept_return,
    )
    .expect("bind");
    let status = run_vm(&mut vm).expect("valid handle return should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(handle)]);
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

    let handle = real_handle_value();
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

    LOOP_RETURN.store(handle, Ordering::SeqCst);
    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact_static_non_yielding_args(&imported.name, 1, schema, static_loop_return)
        .expect("register resource exact non-yielding");
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
