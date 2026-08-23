//! C2/C2 exact manual host-call resource contract integration tests.
//!
//! Scope: the single `ExactHostCallContract` wrapping every VM-aware exact
//! registration (`HostFunctionRegistry::register_exact{,_static,_stack,
//! _static_stack}`) with resource-passing parameters:
//!
//! 1. **Preflight** (`build` + `validate`) runs *before* the user function:
//!    handle structure / arena / generation / slot key / open / not-taken /
//!    ownership / children / pending-operation / same-handle alias conflicts.
//!    A bad argument is a structured error with **zero** user-function calls
//!    and **zero** resource mutation (no close, no ownership/generation
//!    change).
//! 2. **Commit** runs *after* the call: every declared `TakeOwned` must have
//!    moved GuestOwned -> Taken by this invocation; a still-guest-owned one is
//!    safely reclaimed (close fired) and reported as `ResourceNotConsumed`.
//!    Consumed `Borrow`/`BorrowMut` arguments are a structured conflict. The
//!    original host error is preserved; a panicking host still runs cleanup.
//! 3. Registration rejects schemas that are not directly addressable by the
//!    `Value::Int` handle ABI (aggregate-nested resources), rejects *any*
//!    resource passing through args-only (non-VM-aware) registrations, and
//!    bounds schema walks at depth 64 (65 is a structured rejection).
//!
//! A VM-scope handle can only be created after a `Vm` exists, so argument
//! tests inject the borrow/take handle through `acme::ping` — an exact
//! `Resource(io.file)` return — whose host records the pushed handle + its
//! close counter into a per-test static. The guarded function under test then
//! receives a real, in-scope handle.
//!
//! Handles are produced by real pushes into the VM's execution scope; exact
//! schemas + fingerprints come from a real catalog + compiler.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use vm::compiler::{CompileSourceFileOptions, SourceFlavor, TypeSchema};
use vm::resource::{
    CloseProgress, HostResource, ResourceCloseReason, ResourceErrorCode, ResourceResult,
    ResourceTable,
};
use vm::{
    BytecodeBuilder, CallOutcome, CallReturn, HostApiBuilder, HostApiCatalog, HostArgsFunction,
    HostFunction, HostFunctionRegistry, HostFunctionSchema, HostImport, HostImportBindingError,
    HostImportParam, HostImportSchema, HostParamPassing, HostParamSchema, HostTypeSchema,
    JitConfig, Program, Resource, ResourceAccessRequest, ResourceHandle, ResourceOwnership,
    ResourceTypeKey, ResourceTypeSchema, Value, Vm, VmError, VmStatus,
    compile_source_with_flavor_and_options,
};

// ---- resources ------------------------------------------------------------

fn file_key() -> ResourceTypeKey {
    ResourceTypeKey::new("io.file").expect("valid key")
}

fn block_key() -> ResourceTypeKey {
    ResourceTypeKey::new("io.block").expect("valid key")
}

#[derive(Debug)]
struct FileResource {
    value: i64,
    closes: Arc<AtomicUsize>,
}

impl HostResource for FileResource {
    fn resource_type_key() -> Option<ResourceTypeKey>
    where
        Self: Sized,
    {
        Some(file_key())
    }

    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        Ok(CloseProgress::Ready)
    }
}

#[derive(Debug)]
struct BlockResource {
    closes: Arc<AtomicUsize>,
}

impl HostResource for BlockResource {
    fn resource_type_key() -> Option<ResourceTypeKey>
    where
        Self: Sized,
    {
        Some(block_key())
    }

    fn begin_close(&mut self, _reason: ResourceCloseReason) -> ResourceResult<CloseProgress> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        Ok(CloseProgress::Ready)
    }
}

/// A close propagation hook run by the `PingHost` family so a "late close"
/// (a handle closed after it already closed) is impossible by construction.
#[derive(Clone, Copy)]
struct NoopOperation;

impl vm::operation::HostOperation for NoopOperation {
    fn poll(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<vm::operation::OperationResult<()>> {
        std::task::Poll::Pending
    }

    fn cancel(
        &mut self,
        _reason: vm::operation::OperationCancelReason,
    ) -> vm::operation::OperationResult<()> {
        Ok(())
    }
}

// ---- catalog + compiler -----------------------------------------------------

/// Catalog exposing `acme::ping(int) -> io.file` and `acme::create_block(int)
/// -> io.block` returns, plus TakeOwned / borrow resource-parameter functions.
fn catalog() -> Arc<HostApiCatalog> {
    let file = file_key();
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(file.clone(), "file"));
    builder.resource(ResourceTypeSchema::new(block_key(), "block"));

    builder.function(HostFunctionSchema::with_return(
        "acme::ping",
        vec![HostParamSchema::value("v", HostTypeSchema::Int)],
        HostTypeSchema::Resource(file.clone()),
    ));
    builder.function(HostFunctionSchema::with_return(
        "acme::create_block",
        vec![HostParamSchema::value("v", HostTypeSchema::Int)],
        HostTypeSchema::Resource(block_key()),
    ));
    // take(f: TakeOwned) -> Int
    builder.function(HostFunctionSchema::with_return(
        "acme::take",
        vec![HostParamSchema::with_passing(
            "f",
            HostTypeSchema::Resource(file.clone()),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::Int,
    ));
    // take2(a: TakeOwned, b: TakeOwned) -> Int
    builder.function(HostFunctionSchema::with_return(
        "acme::take2",
        vec![
            HostParamSchema::with_passing(
                "a",
                HostTypeSchema::Resource(file.clone()),
                HostParamPassing::TakeOwned,
            ),
            HostParamSchema::with_passing(
                "b",
                HostTypeSchema::Resource(file.clone()),
                HostParamPassing::TakeOwned,
            ),
        ],
        HostTypeSchema::Int,
    ));
    // mix(t: TakeOwned, b: Borrow) -> Int
    builder.function(HostFunctionSchema::with_return(
        "acme::mix",
        vec![
            HostParamSchema::with_passing(
                "t",
                HostTypeSchema::Resource(file.clone()),
                HostParamPassing::TakeOwned,
            ),
            HostParamSchema::with_passing(
                "b",
                HostTypeSchema::Resource(file.clone()),
                HostParamPassing::Borrow,
            ),
        ],
        HostTypeSchema::Int,
    ));
    // `maybe(int) -> Optional<Resource>` for the Null-return contract.
    builder.function(HostFunctionSchema::with_return(
        "acme::maybe",
        vec![HostParamSchema::value("v", HostTypeSchema::Int)],
        HostTypeSchema::Optional(Box::new(HostTypeSchema::Resource(file))),
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

/// Compiles a program that references `name` and returns its exact
/// `HostImport` (schema + real catalog fingerprint).
fn compiled_import(name: &str, source: &str) -> HostImport {
    let compiled = compile_catalog_program(source);
    compiled
        .program
        .imports
        .into_iter()
        .find(|import| import.name == name)
        .unwrap_or_else(|| panic!("import {name} not found"))
}

// ---- programs ---------------------------------------------------------------

/// Program that calls `import` (index 0) exactly once with `args` as Int
/// constants and returns.
fn call_program(import: &HostImport, args: &[i64]) -> Program {
    let mut bc = BytecodeBuilder::new();
    for (index, _) in args.iter().enumerate() {
        bc.ldc(index as u32);
    }
    bc.call(0, args.len() as u8);
    bc.ret();
    let constants = args.iter().map(|&value| Value::Int(value)).collect();
    Program::with_imports_and_debug(constants, bc.finish(), vec![import.clone()], None)
}

/// Program over a 2-import program: first imports[0] (`ping`) produces one
/// handle into local 0, then imports[1] (`take`-style) is called with
/// `take_args` (each an index into local 0).
fn ping_then_call_program(ping: &HostImport, target: &HostImport, arity: u8) -> Program {
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.call(0, 1);
    bc.stloc(0);
    for _ in 0..arity {
        bc.ldloc(0);
    }
    bc.call(1, arity);
    bc.ret();
    Program::with_imports_and_debug(
        vec![Value::Int(7)],
        bc.finish(),
        vec![ping.clone(), target.clone()],
        None,
    )
    .with_local_count(if arity > 1 { arity as usize } else { 1 })
}

/// Program over a 2-import program that calls `ping` twice into locals 0,1
/// and `target` once with both (arity 2).
fn ping2_then_call_program(ping: &HostImport, target: &HostImport) -> Program {
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.call(0, 1);
    bc.stloc(0);
    bc.ldc(0);
    bc.call(0, 1);
    bc.stloc(1);
    bc.ldloc(0);
    bc.ldloc(1);
    bc.call(1, 2);
    bc.ret();
    Program::with_imports_and_debug(
        vec![Value::Int(7)],
        bc.finish(),
        vec![ping.clone(), target.clone()],
        None,
    )
    .with_local_count(2)
}

/// Program that calls `ping` once into local 0, then `target` twice with the
/// same handle: the second call re-passes the stale raw handle (old-Taken).
fn ping_then_take_twice_program(ping: &HostImport, target: &HostImport) -> Program {
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.call(0, 1);
    bc.stloc(0);
    bc.ldloc(0);
    bc.call(1, 1);
    bc.pop();
    bc.ldloc(0);
    bc.call(1, 1);
    bc.ret();
    Program::with_imports_and_debug(
        vec![Value::Int(7)],
        bc.finish(),
        vec![ping.clone(), target.clone()],
        None,
    )
    .with_local_count(1)
}

/// Registers `ping` (exact io.file return) with `ping_host` plus `target`
/// (exact TakeOwned/borrow schema) with `target_static`, binds, and runs.
fn bind_and_run_two_import(
    ping: &HostImport,
    target: &HostImport,
    ping_host: impl Fn() -> Box<dyn HostFunction> + Send + Sync + 'static,
    target_static: fn(&mut Vm, &[Value]) -> vm::VmResult<CallOutcome>,
    program: Program,
) -> (Vm, vm::VmResult<VmStatus>) {
    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact(
            &ping.name,
            1,
            ping.schema.clone().expect("ping schema"),
            ping_host,
        )
        .expect("register ping");
    registry
        .register_exact_static(
            &target.name,
            target.arity,
            target.schema.clone().expect("target schema"),
            target_static,
        )
        .expect("register target");
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    registry.bind_vm_cached(&mut vm).expect("bind");
    let result = vm.run();
    (vm, result)
}

// ---- recording ping hosts ---------------------------------------------------
//
// Each test gets its own host type + static (parallel-safe). The host pushes
// a fresh `FileResource` (or `BlockResource`) into the VM's execution scope,
// records `(raw handle, closes counter)`, and returns the raw handle. The
// exact `Resource(io.file)` return transfer marks it GuestOwned.

macro_rules! recording_ping {
    ($static_name:ident, $host:ident) => {
        static $static_name: std::sync::Mutex<Vec<(u64, Arc<AtomicUsize>)>> =
            std::sync::Mutex::new(Vec::new());
        struct $host;
        impl HostFunction for $host {
            fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> vm::VmResult<CallOutcome> {
                let closes = Arc::new(AtomicUsize::new(0));
                let token = vm
                    .host_context()
                    .push_resource(FileResource {
                        value: 7,
                        closes: closes.clone(),
                    })
                    .expect("push file");
                let raw = token.handle().raw();
                $static_name
                    .lock()
                    .expect("ping record lock")
                    .push((raw, closes));
                Ok(CallOutcome::Return(CallReturn::One(Value::Int(raw as i64))))
            }
        }
    };
}

macro_rules! recording_block_ping {
    ($static_name:ident, $host:ident) => {
        static $static_name: std::sync::Mutex<Vec<(u64, Arc<AtomicUsize>)>> =
            std::sync::Mutex::new(Vec::new());
        struct $host;
        impl HostFunction for $host {
            fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> vm::VmResult<CallOutcome> {
                let closes = Arc::new(AtomicUsize::new(0));
                let token = vm
                    .host_context()
                    .push_resource(BlockResource {
                        closes: closes.clone(),
                    })
                    .expect("push block");
                let raw = token.handle().raw();
                $static_name
                    .lock()
                    .expect("ping record lock")
                    .push((raw, closes));
                Ok(CallOutcome::Return(CallReturn::One(Value::Int(raw as i64))))
            }
        }
    };
}

/// Ping that attaches a child resource to the returned handle (children
/// block a subsequent TakeOwned).
macro_rules! recording_ping_with_child {
    ($static_name:ident, $host:ident) => {
        static $static_name: std::sync::Mutex<Vec<(u64, Arc<AtomicUsize>)>> =
            std::sync::Mutex::new(Vec::new());
        struct $host;
        impl HostFunction for $host {
            fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> vm::VmResult<CallOutcome> {
                let closes = Arc::new(AtomicUsize::new(0));
                let parent = vm
                    .host_context()
                    .push_resource(FileResource {
                        value: 7,
                        closes: closes.clone(),
                    })
                    .expect("push parent");
                let raw = parent.handle().raw();
                vm.host_context()
                    .push_child_resource(
                        FileResource {
                            value: 8,
                            closes: Arc::new(AtomicUsize::new(0)),
                        },
                        &parent,
                    )
                    .expect("push child");
                $static_name
                    .lock()
                    .expect("ping record lock")
                    .push((raw, closes));
                Ok(CallOutcome::Return(CallReturn::One(Value::Int(raw as i64))))
            }
        }
    };
}

/// Ping that associates an active operation with the returned handle (a
/// TakeOwned is blocked while the operation is active).
macro_rules! recording_ping_with_op {
    ($static_name:ident, $host:ident) => {
        static $static_name: std::sync::Mutex<Vec<(u64, Arc<AtomicUsize>)>> =
            std::sync::Mutex::new(Vec::new());
        struct $host;
        impl HostFunction for $host {
            fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> vm::VmResult<CallOutcome> {
                let closes = Arc::new(AtomicUsize::new(0));
                let token = vm
                    .host_context()
                    .push_resource(FileResource {
                        value: 7,
                        closes: closes.clone(),
                    })
                    .expect("push file");
                let raw = token.handle().raw();
                vm.host_context()
                    .start_operation(
                        vm::operation::OperationSpec::new(NoopOperation)
                            .with_resource(token.handle()),
                    )
                    .expect("associate op");
                $static_name
                    .lock()
                    .expect("ping record lock")
                    .push((raw, closes));
                Ok(CallOutcome::Return(CallReturn::One(Value::Int(raw as i64))))
            }
        }
    };
}

// Hosts/statics for each test (unique per test for parallel safety).
recording_ping!(OLD_TAKEN_PING, OldTakenPing);
recording_ping!(DUP_TAKE_PING, DupTakePing);
recording_ping!(TAKE_BORROW_PING, TakeBorrowPing);
recording_block_ping!(WRONG_KEY_BLOCK_PING, WrongKeyBlockPing);
// Per-test JIT/AOT wrong-key producers: `WRONG_KEY_BLOCK_PING` is owned by the
// existing `wrong_key_rejected_zero_calls`, and the JIT and AOT parity tests
// each need their own recording static to stay parallel-safe amongst
// themselves.
recording_block_ping!(JIT_WRONG_KEY_BLOCK_PING, JitWrongKeyBlockPing);
recording_block_ping!(AOT_WRONG_KEY_BLOCK_PING, AotWrongKeyBlockPing);
recording_ping_with_child!(CHILDREN_PING, ChildrenPing);
recording_ping_with_op!(ACTIVE_OP_PING, ActiveOpPing);
recording_ping!(CONSUMED_PING, ConsumedPing);
// Per-test JIT/AOT consumed-ownership producers: `CONSUMED_PING` is owned by
// `taken_owned_consumed_is_ok`, and the JIT and AOT parity tests each get
// their own recording static to stay parallel-safe amongst themselves.
recording_ping!(JIT_CONSUMED_PING, JitConsumedPing);
recording_ping!(AOT_CONSUMED_PING, AotConsumedPing);
recording_ping!(NO_TAKE_PING, NoTakePing);
recording_ping!(ONE_OF_TWO_PING, OneOfTwoPing);
recording_ping!(HOST_ERR_PING, HostErrPing);
recording_ping!(PANIC_PING, PanicPing);
recording_ping!(JIT_LOOP_PING, JitLoopPing);

// Take-side closures, one static counter each.
static TAKE_ONE_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Per-test take counters for the JIT/AOT parity tests (parallel-safe: never
/// touches `TAKE_ONE_CALLS`, which other tests assert on; JIT and AOT each get
/// their own so the two parity tests never perturb each other).
static JIT_TAKE_ONE_CALLS: AtomicUsize = AtomicUsize::new(0);
static AOT_TAKE_ONE_CALLS: AtomicUsize = AtomicUsize::new(0);

fn take_first_arg(vm: &mut Vm, args: &[Value]) -> vm::VmResult<CallOutcome> {
    let handle = ResourceHandle::from_value(&args[0]).map_err(VmError::from)?;
    let frame =
        vm.begin_resource_access(vec![ResourceAccessRequest::take_owned::<FileResource>(
            handle,
        )])?;
    let owned = frame.take_owned::<FileResource>(0)?;
    let value = owned.value;
    drop(frame);
    Ok(CallOutcome::Return(CallReturn::One(Value::Int(value))))
}

fn take_first_arg_counted(vm: &mut Vm, args: &[Value]) -> vm::VmResult<CallOutcome> {
    TAKE_ONE_CALLS.fetch_add(1, Ordering::SeqCst);
    take_first_arg(vm, args)
}

/// `take_first_arg` variants with per-test counters so the parity tests never
/// perturb `TAKE_ONE_CALLS` (JIT and AOT kept separate).
fn jit_take_first_arg_counted(vm: &mut Vm, args: &[Value]) -> vm::VmResult<CallOutcome> {
    JIT_TAKE_ONE_CALLS.fetch_add(1, Ordering::SeqCst);
    take_first_arg(vm, args)
}

fn aot_take_first_arg_counted(vm: &mut Vm, args: &[Value]) -> vm::VmResult<CallOutcome> {
    AOT_TAKE_ONE_CALLS.fetch_add(1, Ordering::SeqCst);
    take_first_arg(vm, args)
}

static NO_TAKE_CALLS: AtomicUsize = AtomicUsize::new(0);

fn no_take_counted(_vm: &mut Vm, _args: &[Value]) -> vm::VmResult<CallOutcome> {
    NO_TAKE_CALLS.fetch_add(1, Ordering::SeqCst);
    Ok(CallOutcome::Return(CallReturn::One(Value::Int(0))))
}

/// `unconsumed_take_owned_reclaims_and_reports_not_consumed` legitimately lets
/// its take host run (the take passes preflight and the callee simply does not
/// consume), so it uses its own counter: the shared `NO_TAKE_CALLS` is never
/// incremented, keeping every zero-asserting preflight test parallel-safe.
static UNCONSUMED_NO_TAKE_CALLS: AtomicUsize = AtomicUsize::new(0);

fn unconsumed_no_take_counted(_vm: &mut Vm, _args: &[Value]) -> vm::VmResult<CallOutcome> {
    UNCONSUMED_NO_TAKE_CALLS.fetch_add(1, Ordering::SeqCst);
    Ok(CallOutcome::Return(CallReturn::One(Value::Int(0))))
}

/// Per-test no-take counters for the JIT/AOT parity tests (parallel-safe:
/// never touches `NO_TAKE_CALLS`, which other tests assert on; JIT and AOT
/// kept separate).
static JIT_NO_TAKE_CALLS: AtomicUsize = AtomicUsize::new(0);
static AOT_NO_TAKE_CALLS: AtomicUsize = AtomicUsize::new(0);

fn jit_no_take_counted(_vm: &mut Vm, _args: &[Value]) -> vm::VmResult<CallOutcome> {
    JIT_NO_TAKE_CALLS.fetch_add(1, Ordering::SeqCst);
    Ok(CallOutcome::Return(CallReturn::One(Value::Int(0))))
}

fn aot_no_take_counted(_vm: &mut Vm, _args: &[Value]) -> vm::VmResult<CallOutcome> {
    AOT_NO_TAKE_CALLS.fetch_add(1, Ordering::SeqCst);
    Ok(CallOutcome::Return(CallReturn::One(Value::Int(0))))
}

// ---- 1. preflight rejection matrix ------------------------------------------

/// A declared TakeOwned argument whose handle is already Taken (consumed by
/// the first call in the program) is rejected *before* the second user call:
/// the old-Taken preflight means a stale raw handle never reaches the callee.
#[test]
fn old_taken_rejected_before_second_call() {
    let ping = compiled_import("acme::ping", "let r = acme::ping(7);\n");
    let take = compiled_import("acme::take", "let r = acme::ping(7); acme::take(r);\n");
    OLD_TAKEN_PING.lock().unwrap().clear();
    TAKE_ONE_CALLS.store(0, Ordering::SeqCst);

    let (_vm, result) = bind_and_run_two_import(
        &ping,
        &take,
        || Box::new(OldTakenPing),
        take_first_arg_counted,
        ping_then_take_twice_program(&ping, &take),
    );

    let error = result.expect_err("re-passing a taken handle must be rejected");
    assert_eq!(
        error.resource_error_code(),
        Some(ResourceErrorCode::ResourceAlreadyTaken),
        "old-Taken handle must be a structured already-taken rejection, got: {error}"
    );
    assert_eq!(
        TAKE_ONE_CALLS.load(Ordering::SeqCst),
        1,
        "only the first take call may invoke the host fn"
    );
}

/// Passing the same handle twice to two `TakeOwned` parameters is rejected in
/// `build` (same-handle alias graph) before any preflight mutation and before
/// the user function runs.
#[test]
fn duplicate_take_owned_aliasing_rejected_zero_calls() {
    let ping = compiled_import("acme::ping", "let r = acme::ping(7);\n");
    let take2 = compiled_import(
        "acme::take2",
        "let r = acme::ping(7); let s = acme::ping(8); acme::take2(r, s);\n",
    );
    DUP_TAKE_PING.lock().unwrap().clear();
    NO_TAKE_CALLS.store(0, Ordering::SeqCst);

    let (mut contract_vm, result) = bind_and_run_two_import(
        &ping,
        &take2,
        || Box::new(DupTakePing),
        no_take_counted,
        ping_then_call_program(&ping, &take2, 2), // both args from the same local!
    );

    let error = result.expect_err("duplicate TakeOwned alias must be rejected");
    assert_eq!(
        error.resource_error_code(),
        Some(ResourceErrorCode::ResourceAccessConflict),
        "duplicate TakeOwned must be a structured access conflict, got: {error}"
    );
    assert_eq!(
        NO_TAKE_CALLS.load(Ordering::SeqCst),
        0,
        "the user function must never run on an alias conflict"
    );
    let (raw, closes) = &DUP_TAKE_PING.lock().unwrap()[0];
    let (raw, closes) = (*raw, closes.clone());
    assert_eq!(closes.load(Ordering::SeqCst), 0, "no close on preflight");
    assert_eq!(
        contract_vm
            .host_context()
            .execution_scope()
            .resources()
            .ownership(ResourceHandle::from_raw(raw).expect("valid handle")),
        Some(ResourceOwnership::GuestOwned),
        "resource untouched by the rejected call (still GuestOwned)"
    );
}

/// A `TakeOwned` argument aliased from a `Borrow` argument of the same handle
/// is rejected structurally before the user function runs.
#[test]
fn take_plus_borrow_alias_rejected_zero_calls() {
    let ping = compiled_import("acme::ping", "let r = acme::ping(7);\n");
    let mix = compiled_import(
        "acme::mix",
        "let r = acme::ping(7); let s = acme::ping(9); acme::mix(r, &s);\n",
    );
    TAKE_BORROW_PING.lock().unwrap().clear();
    NO_TAKE_CALLS.store(0, Ordering::SeqCst);

    let (_contract_vm, result) = bind_and_run_two_import(
        &ping,
        &mix,
        || Box::new(TakeBorrowPing),
        no_take_counted,
        ping_then_call_program(&ping, &mix, 2), // take + borrow from the same local
    );

    let error = result.expect_err("Take+Borrow alias must be rejected");
    assert_eq!(
        error.resource_error_code(),
        Some(ResourceErrorCode::ResourceAccessConflict),
        "Take+Borrow alias must be a structured access conflict, got: {error}"
    );
    assert_eq!(NO_TAKE_CALLS.load(Ordering::SeqCst), 0);
    let (_raw, closes) = {
        let locked = TAKE_BORROW_PING.lock().unwrap();
        let (raw, closes) = locked[0].clone();
        (raw, closes)
    };
    assert_eq!(closes.load(Ordering::SeqCst), 0, "no close on preflight");
}

/// A TakeOwned argument carrying a resource whose live slot key does not match
/// the schema's expected key is a structured `ResourceKeyMismatch` with zero
/// user calls.
#[test]
fn wrong_key_rejected_zero_calls() {
    // The producing import is `acme::create_block` (exact `Resource(io.block)`
    // return) so the block handle enters the scope legal; the `acme::take`
    // target then sees a live slot key `io.block` where it expects `io.file`.
    let create_block = compiled_import("acme::create_block", "let b = acme::create_block(7);\n");
    let take = compiled_import("acme::take", "let r = acme::ping(7); acme::take(r);\n");
    WRONG_KEY_BLOCK_PING.lock().unwrap().clear();
    NO_TAKE_CALLS.store(0, Ordering::SeqCst);

    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact(
            &create_block.name,
            1,
            create_block.schema.clone().expect("schema"),
            || Box::new(WrongKeyBlockPing),
        )
        .expect("register create_block");
    registry
        .register_exact_static(
            &take.name,
            take.arity,
            take.schema.clone().expect("schema"),
            no_take_counted,
        )
        .expect("register take");
    let mut vm = Vm::try_new(ping_then_call_program(&create_block, &take, 1))
        .expect("test VM construction must not fail");
    registry.bind_vm_cached(&mut vm).expect("bind");

    let error = vm.run().expect_err("wrong-key TakeOwned must be rejected");
    assert_eq!(
        error.resource_error_code(),
        Some(ResourceErrorCode::ResourceKeyMismatch),
        "wrong key must be a structured key mismatch, got: {error}"
    );
    assert_eq!(NO_TAKE_CALLS.load(Ordering::SeqCst), 0);
    let closes = {
        let locked = WRONG_KEY_BLOCK_PING.lock().unwrap();
        locked[0].1.clone()
    };
    assert_eq!(
        closes.load(Ordering::SeqCst),
        0,
        "no close on key preflight"
    );
}

/// A handle that decodes structurally but belongs to a foreign arena is a
/// structured `ResourceHandleWrongTable` with zero user calls.
#[test]
fn foreign_handle_rejected_zero_calls() {
    let take = compiled_import("acme::take", "let r = acme::ping(7); acme::take(r);\n");
    let schema = take.schema.clone().expect("exact schema");
    NO_TAKE_CALLS.store(0, Ordering::SeqCst);

    // Structurally-valid handle from a different table (different arena).
    let mut foreign = ResourceTable::new().expect("table");
    let handle = foreign
        .push(FileResource {
            value: 1,
            closes: Arc::new(AtomicUsize::new(0)),
        })
        .expect("push foreign")
        .handle();
    let raw = handle.raw() as i64;

    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact_static(&take.name, 1, schema, no_take_counted)
        .expect("register take");
    let mut vm =
        Vm::try_new(call_program(&take, &[raw])).expect("test VM construction must not fail");
    registry.bind_vm_cached(&mut vm).expect("bind");

    let error = vm.run().expect_err("foreign handle must be rejected");
    assert_eq!(
        error.resource_error_code(),
        Some(ResourceErrorCode::ResourceHandleWrongTable),
        "foreign arena must be a structured wrong-table rejection, got: {error}"
    );
    assert_eq!(NO_TAKE_CALLS.load(Ordering::SeqCst), 0);
}

/// A resource with a live child cannot be taken: the child check runs in the
/// preflight with zero user calls.
#[test]
fn has_children_rejected_zero_calls() {
    let ping = compiled_import("acme::ping", "let r = acme::ping(7);\n");
    let take = compiled_import("acme::take", "let r = acme::ping(7); acme::take(r);\n");
    CHILDREN_PING.lock().unwrap().clear();
    NO_TAKE_CALLS.store(0, Ordering::SeqCst);

    let (_contract_vm, result) = bind_and_run_two_import(
        &ping,
        &take,
        || Box::new(ChildrenPing),
        no_take_counted,
        ping_then_call_program(&ping, &take, 1),
    );

    let error = result.expect_err("take with live children must be rejected");
    assert_eq!(
        error.resource_error_code(),
        Some(ResourceErrorCode::ResourceHasChildren),
        "live children must be a structured has-children rejection, got: {error}"
    );
    assert_eq!(NO_TAKE_CALLS.load(Ordering::SeqCst), 0);
    let closes = {
        let locked = CHILDREN_PING.lock().unwrap();
        locked[0].1.clone()
    };
    assert_eq!(closes.load(Ordering::SeqCst), 0, "no close on preflight");
}

/// A resource associated with an active operation cannot be taken: the
/// operation check runs in the preflight with zero user calls.
#[test]
fn active_operation_rejected_zero_calls() {
    let ping = compiled_import("acme::ping", "let r = acme::ping(7);\n");
    let take = compiled_import("acme::take", "let r = acme::ping(7); acme::take(r);\n");
    ACTIVE_OP_PING.lock().unwrap().clear();
    NO_TAKE_CALLS.store(0, Ordering::SeqCst);

    let (_contract_vm, result) = bind_and_run_two_import(
        &ping,
        &take,
        || Box::new(ActiveOpPing),
        no_take_counted,
        ping_then_call_program(&ping, &take, 1),
    );

    let error = result.expect_err("take with an active operation must be rejected");
    assert_eq!(
        error.resource_error_code(),
        Some(ResourceErrorCode::ResourceOperationActive),
        "active op must be a structured operation-active rejection, got: {error}"
    );
    assert_eq!(NO_TAKE_CALLS.load(Ordering::SeqCst), 0);
}

// ---- 2. commit / cleanup ---------------------------------------------------

/// A declared TakeOwned that is consumed by the host fn is fine: the handle
/// moves GuestOwned -> Taken and the returned value lands on the stack.
#[test]
fn taken_owned_consumed_is_ok() {
    let ping = compiled_import("acme::ping", "let r = acme::ping(7);\n");
    let take = compiled_import("acme::take", "let r = acme::ping(7); acme::take(r);\n");
    CONSUMED_PING.lock().unwrap().clear();

    let (mut vm, result) = bind_and_run_two_import(
        &ping,
        &take,
        || Box::new(ConsumedPing),
        take_first_arg,
        ping_then_call_program(&ping, &take, 1),
    );

    let status = result.expect("consumed take must run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(7)], "returned the resource value");
    let (raw, closes) = {
        let locked = CONSUMED_PING.lock().unwrap();
        (locked[0].0, locked[0].1.clone())
    };
    assert_eq!(
        vm.host_context()
            .execution_scope()
            .resources()
            .ownership(ResourceHandle::from_raw(raw).expect("valid handle")),
        Some(ResourceOwnership::Taken),
        "consumed handle must be Taken"
    );
    assert_eq!(closes.load(Ordering::SeqCst), 0, "taken not closed");
}

/// A declared TakeOwned that is *not* consumed by the host fn returns a
/// structured `ResourceNotConsumed` and safely reclaims (closes) the still
/// guest-owned resource.
#[test]
fn unconsumed_take_owned_reclaims_and_reports_not_consumed() {
    let ping = compiled_import("acme::ping", "let r = acme::ping(7);\n");
    let take = compiled_import("acme::take", "let r = acme::ping(7); acme::take(r);\n");
    NO_TAKE_PING.lock().unwrap().clear();

    let (_vm, result) = bind_and_run_two_import(
        &ping,
        &take,
        || Box::new(NoTakePing),
        unconsumed_no_take_counted,
        ping_then_call_program(&ping, &take, 1),
    );

    let error = result.expect_err("unconsumed take must report resource_not_consumed");
    assert_eq!(
        error.resource_error_code(),
        Some(ResourceErrorCode::ResourceNotConsumed),
        "unconsumed take must be a structured not-consumed error, got: {error}"
    );
    let closes = {
        let locked = NO_TAKE_PING.lock().unwrap();
        locked[0].1.clone()
    };
    assert_eq!(
        closes.load(Ordering::SeqCst),
        1,
        "the unconsumed guest-owned resource must be reclaimed (closed once)"
    );
}

/// With two declared TakeOwned args, consuming exactly one leaves the other
/// guest-owned -> `ResourceNotConsumed`, the consumed handle stays Taken and
/// the unconsumed one is reclaimed.
#[test]
fn one_of_two_consumed_errors_and_reclaims_second() {
    let ping = compiled_import("acme::ping", "let r = acme::ping(7);\n");
    let take2 = compiled_import(
        "acme::take2",
        "let r = acme::ping(7); let s = acme::ping(8); acme::take2(r, s);\n",
    );
    ONE_OF_TWO_PING.lock().unwrap().clear();

    let (mut vm, result) = bind_and_run_two_import(
        &ping,
        &take2,
        || Box::new(OneOfTwoPing),
        take_first_arg, // VM-aware host consumes ONLY the first argument
        ping2_then_call_program(&ping, &take2),
    );

    let error = result.expect_err("unconsumed second take must be reported");
    assert_eq!(
        error.resource_error_code(),
        Some(ResourceErrorCode::ResourceNotConsumed),
        "got: {error}"
    );
    let records = ONE_OF_TWO_PING.lock().unwrap().clone();
    let (raw_first, closes_first) = &records[0];
    let (_, closes_second) = &records[1];
    let (raw_first, closes_first) = (*raw_first, closes_first.clone());
    let closes_second = closes_second.clone();
    assert_eq!(
        vm.host_context()
            .execution_scope()
            .resources()
            .ownership(ResourceHandle::from_raw(raw_first).expect("valid handle")),
        Some(ResourceOwnership::Taken),
        "consumed first handle stays Taken"
    );
    assert_eq!(
        closes_first.load(Ordering::SeqCst),
        0,
        "consumed not closed"
    );
    assert_eq!(
        closes_second.load(Ordering::SeqCst),
        1,
        "unconsumed second handle reclaimed"
    );
}

/// When the host fn returns `Err`, the original error is preserved, taken
/// values stay Taken, and any still-guest-owned declared take is reclaimed
/// without masking the primary error.
#[test]
fn host_error_preserves_original_and_reclaims_unconsumed() {
    use vm::VmResult;

    // (a) host consumes then fails: take stays Taken, original error wins.
    {
        let ping = compiled_import("acme::ping", "let r = acme::ping(7);\n");
        let take = compiled_import("acme::take", "let r = acme::ping(7); acme::take(r);\n");
        HOST_ERR_PING.lock().unwrap().clear();
        static ERR_TAKE_CALLS: AtomicUsize = AtomicUsize::new(0);
        ERR_TAKE_CALLS.store(0, Ordering::SeqCst);

        fn err_after_take(vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
            ERR_TAKE_CALLS.fetch_add(1, Ordering::SeqCst);
            let handle = ResourceHandle::from_value(&args[0]).map_err(VmError::from)?;
            let frame = vm.begin_resource_access(vec![ResourceAccessRequest::take_owned::<
                FileResource,
            >(handle)])?;
            let _owned = frame.take_owned::<FileResource>(0)?;
            drop(frame);
            Err(VmError::HostError("boom".to_string()))
        }

        let (mut vm, result) = bind_and_run_two_import(
            &ping,
            &take,
            || Box::new(HostErrPing),
            err_after_take,
            ping_then_call_program(&ping, &take, 1),
        );

        assert!(
            matches!(result, Err(VmError::HostError(ref message)) if message == "boom"),
            "the original host error must be reported, got: {result:?}"
        );
        assert_eq!(ERR_TAKE_CALLS.load(Ordering::SeqCst), 1);
        let (raw, closes) = {
            let locked = HOST_ERR_PING.lock().unwrap();
            (locked[0].0, locked[0].1.clone())
        };
        assert_eq!(
            vm.host_context()
                .execution_scope()
                .resources()
                .ownership(ResourceHandle::from_raw(raw).expect("valid handle")),
            Some(ResourceOwnership::Taken),
            "the take performed before the error stays Taken"
        );
        assert_eq!(closes.load(Ordering::SeqCst), 0, "taken not closed");
    }

    // (b) host does not consume and fails: original error wins and the
    //     unconsumed resource is reclaimed.
    {
        static ERR_NO_TAKE_CALLS: AtomicUsize = AtomicUsize::new(0);
        ERR_NO_TAKE_CALLS.store(0, Ordering::SeqCst);
        fn err_no_take(_vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
            ERR_NO_TAKE_CALLS.fetch_add(1, Ordering::SeqCst);
            Err(VmError::HostError("boom".to_string()))
        }

        let ping = compiled_import("acme::ping", "let r = acme::ping(7);\n");
        let take = compiled_import("acme::take", "let r = acme::ping(7); acme::take(r);\n");
        HOST_ERR_PING.lock().unwrap().clear();

        let (_vm, result) = bind_and_run_two_import(
            &ping,
            &take,
            || Box::new(HostErrPing),
            err_no_take,
            ping_then_call_program(&ping, &take, 1),
        );

        assert!(
            matches!(result, Err(VmError::HostError(ref message)) if message == "boom"),
            "original error must win over cleanup, got: {result:?}"
        );
        let closes = {
            let locked = HOST_ERR_PING.lock().unwrap();
            locked[0].1.clone()
        };
        assert_eq!(
            closes.load(Ordering::SeqCst),
            1,
            "unconsumed guest-owned resource reclaimed after host error"
        );
    }
}

/// A panicking host function still runs the post-call cleanup (no leak): the
/// still-guest-owned declared take is reclaimed even though the call unwound.
#[test]
fn host_panic_runs_post_contract_and_reclaims() {
    let ping = compiled_import("acme::ping", "let r = acme::ping(7);\n");
    let take = compiled_import("acme::take", "let r = acme::ping(7); acme::take(r);\n");
    PANIC_PING.lock().unwrap().clear();

    fn no_take_panic(_vm: &mut Vm, _args: &[Value]) -> vm::VmResult<CallOutcome> {
        panic!("host panic boom");
    }

    // Register + bind like the harness, but keep `vm.run()` inside the
    // catch_unwind: the guarded call resumes the host unwind.
    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact(
            &ping.name,
            1,
            ping.schema.clone().expect("ping schema"),
            || Box::new(PanicPing),
        )
        .expect("register ping");
    registry
        .register_exact_static(
            &take.name,
            take.arity,
            take.schema.clone().expect("take schema"),
            no_take_panic,
        )
        .expect("register take");
    let mut vm = Vm::try_new(ping_then_call_program(&ping, &take, 1))
        .expect("test VM construction must not fail");
    registry.bind_vm_cached(&mut vm).expect("bind");

    let payload = catch_unwind(AssertUnwindSafe(|| vm.run()))
        .expect_err("host panic must propagate out of run");
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("?");
    assert!(message.contains("boom"), "unexpected panic: {message}");
    let closes = {
        let locked = PANIC_PING.lock().unwrap();
        locked[0].1.clone()
    };
    assert_eq!(
        closes.load(Ordering::SeqCst),
        1,
        "post-call cleanup must run even on panic"
    );
    drop(vm);
}

// ---- 3. return key / Optional Null -----------------------------------------

/// An exact `Resource(io.file)` return whose handle carries a *different* key
/// is a structured `ResourceKeyMismatch`; the handle stays host-owned and the
/// stack is untouched (atomic).
#[test]
fn return_key_mismatch_keeps_host_owned_and_stack_atomic() {
    let ping = compiled_import("acme::ping", "let r = acme::ping(7); r;\n");
    let schema = ping.schema.clone().expect("exact schema");

    // Host pushes a BlockResource (key io.block) and returns its handle: the
    // exact-return transfer must reject the key mismatch.
    struct ReturnBlockHost;
    impl HostFunction for ReturnBlockHost {
        fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> vm::VmResult<CallOutcome> {
            let token = vm
                .host_context()
                .push_resource(BlockResource {
                    closes: Arc::new(AtomicUsize::new(0)),
                })
                .expect("push block");
            Ok(CallOutcome::Return(CallReturn::One(Value::Int(
                token.handle().raw() as i64,
            ))))
        }
    }

    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact(&ping.name, 1, schema, || Box::new(ReturnBlockHost))
        .expect("register ping");
    let mut vm =
        Vm::try_new(call_program(&ping, &[7])).expect("test VM construction must not fail");
    registry.bind_vm_cached(&mut vm).expect("bind");

    let error = vm.run().expect_err("wrong-key return must be rejected");
    assert_eq!(
        error.resource_error_code(),
        Some(ResourceErrorCode::ResourceKeyMismatch),
        "wrong-key return must be a structured key mismatch, got: {error}"
    );
    assert_eq!(
        vm.stack(),
        &[Value::Int(7)],
        "a rejected exact return preserves the pre-call snapshot (no handle pushed)"
    );
}

/// An `Optional<Resource>` exact return with `Null` is legal: `Null` is pushed
/// and no ownership transfer runs.
#[test]
fn optional_resource_return_null_is_legal() {
    let maybe = compiled_import("acme::maybe", "let m = acme::maybe(7); m;\n");
    let schema = maybe.schema.clone().expect("exact schema");

    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact_static_non_yielding_args(&maybe.name, 1, schema, |_| {
            Ok(CallOutcome::Return(CallReturn::One(Value::Null)))
        })
        .expect("register maybe");
    let mut vm =
        Vm::try_new(call_program(&maybe, &[7])).expect("test VM construction must not fail");
    registry.bind_vm_cached(&mut vm).expect("bind");

    let status = vm.run().expect("Null optional return must be legal");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Null]);
}

// ---- 4. registration-time rejections ---------------------------------------

/// Args-only exact registrations reject ANY resource passing, even an
/// otherwise directly-addressable TakeOwned (no `&mut Vm` to enforce the
/// contract); nonresource args registrations stay allowed.
#[test]
fn args_registration_rejects_resource_passing() {
    let take = compiled_import("acme::take", "let r = acme::ping(7); acme::take(r);\n");
    let schema = take.schema.clone().expect("exact schema");

    let mut registry = HostFunctionRegistry::new();
    let error = registry
        .register_exact_args(&take.name, 1, schema, || Box::new(NoopArgsHost))
        .expect_err("Args-only TakeOwned must be rejected at registration");
    assert!(
        matches!(
            error,
            VmError::HostImportBinding(HostImportBindingError::InvalidSchema { .. })
        ),
        "expected structured InvalidSchema, got: {error}"
    );
}

/// Nonresource Args host used to prove args-only registration still works for
/// resource-free schemas (it is never reached here — registration rejects).
struct NoopArgsHost;
impl HostArgsFunction for NoopArgsHost {
    fn call(&mut self, _args: &[Value]) -> vm::VmResult<CallOutcome> {
        Ok(CallOutcome::Return(CallReturn::One(Value::Int(0))))
    }
}

/// Registration-time depth bound: a schema nested at depth 64 passes, depth 65
/// is a structured rejection.
#[test]
fn schema_depth_64_ok_65_rejected() {
    fn nested_optional(depth: u8) -> TypeSchema {
        let mut schema = TypeSchema::Int;
        for _ in 0..depth {
            schema = TypeSchema::Optional(Box::new(schema));
        }
        schema
    }
    fn make_schema(param_schema: TypeSchema) -> HostImportSchema {
        let fp = catalog().fingerprint();
        HostImportSchema {
            params: vec![HostImportParam {
                name: "v".into(),
                schema: param_schema,
                passing: HostParamPassing::Value,
            }],
            return_type: TypeSchema::Int,
            fingerprint: fp,
        }
    }

    // Depth 64: fine.
    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact_static(
            "depth::ok",
            1,
            make_schema(nested_optional(64)),
            |_vm, _args| Ok(CallOutcome::Return(CallReturn::One(Value::Int(0)))),
        )
        .expect("depth 64 schema must register");

    // Depth 65: structured rejection.
    let error = registry
        .register_exact_static(
            "depth::too_deep",
            1,
            make_schema(nested_optional(65)),
            |_vm, _args| Ok(CallOutcome::Return(CallReturn::One(Value::Int(0)))),
        )
        .expect_err("depth 65 schema must be rejected at registration");
    assert!(
        matches!(
            error,
            VmError::HostImportBinding(HostImportBindingError::InvalidSchema { .. })
        ),
        "expected structured InvalidSchema for depth 65, got: {error}"
    );
}

/// A resource nested inside an aggregate (`Optional<Optional<Resource>>`) is
/// not directly addressable and is rejected at registration.
#[test]
fn aggregate_nested_resource_rejected_at_registration() {
    let file = file_key();
    let nested = TypeSchema::Optional(Box::new(TypeSchema::Optional(Box::new(
        TypeSchema::Resource(file),
    ))));
    let schema = HostImportSchema {
        params: vec![HostImportParam {
            name: "f".into(),
            schema: nested,
            passing: HostParamPassing::TakeOwned,
        }],
        return_type: TypeSchema::Int,
        fingerprint: catalog().fingerprint(),
    };

    let mut registry = HostFunctionRegistry::new();
    let error = registry
        .register_exact_static("acme::bad", 1, schema, |_vm, _args| {
            Ok(CallOutcome::Return(CallReturn::One(Value::Int(0))))
        })
        .expect_err("aggregate-nested resource must be rejected at registration");
    assert!(
        matches!(
            error,
            VmError::HostImportBinding(HostImportBindingError::InvalidSchema { .. })
        ),
        "expected structured InvalidSchema, got: {error}"
    );
}

/// An aggregate-nested resource **return** (`Array<Resource>` or
/// `Optional<Optional<... Resource ...>>`) is rejected at registration too;
/// only `Resource(key)` and a single `Optional<Resource(key)>` may carry a
/// resource across the boundary.
#[test]
fn aggregate_nested_resource_return_rejected_at_registration() {
    let file = file_key();
    let mut registry = HostFunctionRegistry::new();
    let ok =
        |_vm: &mut Vm, _args: &[Value]| Ok(CallOutcome::Return(CallReturn::One(Value::Int(0))));

    // Legal: direct Resource(io.file) return.
    let direct = HostImportSchema {
        params: vec![],
        return_type: TypeSchema::Resource(file.clone()),
        fingerprint: catalog().fingerprint(),
    };
    registry
        .register_exact_static("acme::direct", 0, direct, ok)
        .expect("direct Resource return must register");

    // Legal: Optional<Resource(io.file)> return.
    let optional = HostImportSchema {
        params: vec![],
        return_type: TypeSchema::Optional(Box::new(TypeSchema::Resource(file.clone()))),
        fingerprint: catalog().fingerprint(),
    };
    registry
        .register_exact_static("acme::optional", 0, optional, ok)
        .expect("Optional<Resource> return must register");

    // Rejected: Array<Resource>.
    let array = HostImportSchema {
        params: vec![],
        return_type: TypeSchema::Array(Box::new(TypeSchema::Resource(file.clone()))),
        fingerprint: catalog().fingerprint(),
    };
    let error = registry
        .register_exact_static("acme::array_bad", 0, array, ok)
        .expect_err("Array<Resource> return must be rejected at registration");
    assert!(
        matches!(
            error,
            VmError::HostImportBinding(HostImportBindingError::InvalidSchema { .. })
        ),
        "expected structured InvalidSchema for Array<Resource> return, got: {error}"
    );

    // Rejected: Optional<Optional<Resource>> (aggregate nested).
    let nested = HostImportSchema {
        params: vec![],
        return_type: TypeSchema::Optional(Box::new(TypeSchema::Optional(Box::new(
            TypeSchema::Resource(file),
        )))),
        fingerprint: catalog().fingerprint(),
    };
    let error = registry
        .register_exact_static("acme::nested_bad", 0, nested, ok)
        .expect_err("nested-Optional resource return must be rejected at registration");
    assert!(
        matches!(
            error,
            VmError::HostImportBinding(HostImportBindingError::InvalidSchema { .. })
        ),
        "expected structured InvalidSchema for nested-Optional resource return, got: {error}"
    );
}

// ---- review finding 4: JIT / AOT call-boundary parity -----------------------

/// Matches the native-backend gate used by the JIT/AOT integration suite.
/// When no backend is available the deterministic inline-gate tests in
/// `src/vm/host.rs` still prove the resource imports stay off the native
/// inline shim; the behavioral runs here are additionally gated so they never
/// depend on a compiler that the test host cannot load.
fn native_backend_supported() -> bool {
    (cfg!(target_arch = "x86_64")
        && (cfg!(target_os = "windows") || (cfg!(unix) && !cfg!(target_os = "macos"))))
        || (cfg!(target_arch = "aarch64")
            && (cfg!(target_os = "linux") || cfg!(target_os = "macos")))
}

fn patch_branch_target(code: &mut [u8], instr_ip: u32, target: u32) {
    let start = instr_ip as usize + 1;
    code[start..start + 4].copy_from_slice(&target.to_le_bytes());
}

/// A pure-arithmetic loop that reliably compiles a native trace when the
/// backend is available; proves the JIT engine is genuinely executing natively
/// in this test before we assert anything about resource calls on that engine.
///
/// Constant pool: [0, 1, 64] — `ldc(2)` pushes the loop limit 64.
fn arithmetic_loop_program() -> Program {
    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.stloc(0);
    let loop_ip = bc.position();
    bc.ldloc(0);
    bc.ldc(2);
    bc.clt();
    let exit_branch_ip = bc.position();
    bc.brfalse(0);
    bc.ldloc(0);
    bc.ldc(1);
    bc.add();
    bc.stloc(0);
    bc.br(loop_ip);
    let exit_ip = bc.position();
    bc.ldloc(0);
    bc.ret();
    let mut code = bc.finish();
    patch_branch_target(&mut code, exit_branch_ip, exit_ip);
    Program::new(vec![Value::Int(0), Value::Int(1), Value::Int(64)], code).with_local_count(1)
}

fn with_jit(vm: &mut Vm) {
    vm.set_jit_config(JitConfig {
        enabled: true,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    });
}

/// Enabling the JIT must not change the exact contract: the resource-carrying
/// import is never a non-yielding inline shim (deterministically proven in
/// `src/vm/host.rs`  `jit_import_is_inline_eligible` /
/// `jit_sync_flags_mark_resource_return_import_non_inline`), so the wrong-key
/// rejection and TakeOwned consumption keep byte-identical outcomes when the
/// engine is turned on.
#[test]
fn jit_enabled_preserves_wrong_key_and_take_owned_contract() {
    // Wrong key stays a structured rejection with zero user calls.
    let create_block = compiled_import("acme::create_block", "let b = acme::create_block(7);\n");
    let take = compiled_import("acme::take", "let r = acme::ping(7); acme::take(r);\n");
    JIT_WRONG_KEY_BLOCK_PING.lock().unwrap().clear();
    JIT_NO_TAKE_CALLS.store(0, Ordering::SeqCst);
    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact(
            &create_block.name,
            1,
            create_block.schema.clone().expect("schema"),
            || Box::new(JitWrongKeyBlockPing),
        )
        .expect("register create_block");
    registry
        .register_exact_static(
            &take.name,
            take.arity,
            take.schema.clone().expect("schema"),
            jit_no_take_counted,
        )
        .expect("register take");
    let mut vm = Vm::try_new(ping_then_call_program(&create_block, &take, 1))
        .expect("test VM construction must not fail");
    registry.bind_vm_cached(&mut vm).expect("bind");
    with_jit(&mut vm);
    let error = vm
        .run()
        .expect_err("wrong-key TakeOwned stays rejected under JIT");
    assert_eq!(
        error.resource_error_code(),
        Some(ResourceErrorCode::ResourceKeyMismatch),
        "wrong key must stay a structured key mismatch under JIT, got: {error}"
    );
    assert_eq!(JIT_NO_TAKE_CALLS.load(Ordering::SeqCst), 0);
    let closes = JIT_WRONG_KEY_BLOCK_PING.lock().unwrap()[0].1.clone();
    assert_eq!(
        closes.load(Ordering::SeqCst),
        0,
        "no close on key preflight"
    );

    // A consumed TakeOwned stays consumed (GuestOwned -> Taken, zero closes).
    // (Built directly, not via `bind_and_run_two_import`, because JIT must be
    // enabled before the run.)
    let ping = compiled_import("acme::ping", "let r = acme::ping(7);\n");
    let take = compiled_import("acme::take", "let r = acme::ping(7); acme::take(r);\n");
    JIT_CONSUMED_PING.lock().unwrap().clear();
    JIT_TAKE_ONE_CALLS.store(0, Ordering::SeqCst);
    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact(&ping.name, 1, ping.schema.clone().expect("schema"), || {
            Box::new(JitConsumedPing)
        })
        .expect("register ping");
    registry
        .register_exact_static(
            &take.name,
            take.arity,
            take.schema.clone().expect("schema"),
            jit_take_first_arg_counted,
        )
        .expect("register take");
    let mut vm = Vm::try_new(ping_then_call_program(&ping, &take, 1))
        .expect("test VM construction must not fail");
    registry.bind_vm_cached(&mut vm).expect("bind");
    with_jit(&mut vm);
    let status = vm.run().expect("consumed take under JIT");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(7)]);
    let (raw, closes) = {
        let locked = JIT_CONSUMED_PING.lock().unwrap();
        (locked[0].0, locked[0].1.clone())
    };
    assert_eq!(
        vm.host_context()
            .execution_scope()
            .resources()
            .ownership(ResourceHandle::from_raw(raw).expect("valid handle")),
        Some(ResourceOwnership::Taken),
        "handle must be Taken after a JIT-run consume"
    );
    assert_eq!(closes.load(Ordering::SeqCst), 0, "taken not closed");
}

/// When a native backend exists, prove it engages at all, and that a hot loop
/// around a resource-producing exact call leaves every returned handle
/// guest-owned with zero closes (the exact-return transfer happens at the
/// interpreter boundary, never inside native code).
#[test]
fn jit_native_loop_preserves_exact_resource_return_ownership() {
    if !native_backend_supported() {
        return;
    }

    // Prove the engine really runs natively with a host-call-free loop.
    let mut vm =
        Vm::try_new(arithmetic_loop_program()).expect("test VM construction must not fail");
    with_jit(&mut vm);
    let status = vm.run().expect("arithmetic loop under JIT");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(64)]);
    assert!(
        vm.jit_native_exec_count() > 0,
        "native JIT must actually execute the hot loop, dump:\n{}",
        vm.dump_jit_info()
    );
    drop(vm);

    // Now the exact-return contract in the same natively-compiling loop: call
    // `acme::ping` (exact Resource(io.file) return) 32 times; every returned
    // handle must be structurally validated, marked guest-owned, and never
    // closed by the native path.
    let ping = compiled_import("acme::ping", "let r = acme::ping(7);\n");
    JIT_LOOP_PING.lock().unwrap().clear();

    let mut bc = BytecodeBuilder::new();
    bc.ldc(0);
    bc.stloc(0);
    let loop_ip = bc.position();
    bc.ldloc(0);
    bc.ldc(2);
    bc.clt();
    let exit_branch_ip = bc.position();
    bc.brfalse(0);
    bc.ldc(0);
    bc.call(0, 1);
    bc.pop();
    bc.ldloc(0);
    bc.ldc(1);
    bc.add();
    bc.stloc(0);
    bc.br(loop_ip);
    let exit_ip = bc.position();
    bc.ldc(0);
    bc.ret();
    let mut code = bc.finish();
    patch_branch_target(&mut code, exit_branch_ip, exit_ip);
    let program = Program::with_imports_and_debug(
        vec![Value::Int(0), Value::Int(1), Value::Int(32)],
        code,
        vec![ping.clone()],
        None,
    )
    .with_local_count(1);

    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact(&ping.name, 1, ping.schema.clone().expect("schema"), || {
            Box::new(JitLoopPing)
        })
        .expect("register ping");
    let mut vm = Vm::try_new(program).expect("test VM construction must not fail");
    registry.bind_vm_cached(&mut vm).expect("bind");
    with_jit(&mut vm);
    let status = vm.run().expect("resource loop under JIT");
    assert_eq!(status, VmStatus::Halted);
    assert!(
        vm.jit_native_exec_count() > 0,
        "the non-resource loop body should have compiled and run natively,\
         dump:\n{}",
        vm.dump_jit_info()
    );

    let records = JIT_LOOP_PING.lock().unwrap();
    assert_eq!(records.len(), 32, "each loop iteration produced a handle");
    for (raw, closes) in records.iter() {
        assert_eq!(
            vm.host_context()
                .execution_scope()
                .resources()
                .ownership(ResourceHandle::from_raw(*raw).expect("valid handle")),
            Some(ResourceOwnership::GuestOwned),
            "every exact-returned handle must be guest-owned"
        );
        assert_eq!(closes.load(Ordering::SeqCst), 0, "no close mid-run");
    }
    drop(records);

    // Exiting the scope (explicit close + drive) closes each guest-owned
    // handle exactly once.
    let mut cx = vm.host_context();
    cx.begin_close(ResourceCloseReason::Requested)
        .expect("scope begin close");
    let waker = std::task::Waker::from(std::sync::Arc::new(NoopWaker(0)));
    let mut context = std::task::Context::from_waker(&waker);
    loop {
        match cx.poll_close(&mut context) {
            std::task::Poll::Pending => continue,
            std::task::Poll::Ready(Ok(_)) => break,
            std::task::Poll::Ready(Err(error)) => panic!("scope close failed: {error}"),
        }
    }
    drop(cx);
    drop(vm);
    let records = JIT_LOOP_PING.lock().unwrap();
    for (_, closes) in records.iter() {
        assert_eq!(closes.load(Ordering::SeqCst), 1, "exactly-once close");
    }
}

struct NoopWaker(usize);
impl std::task::Wake for NoopWaker {
    fn wake(self: std::sync::Arc<Self>) {}
}

/// AOT-installed execution must preserve the same exact contract at every call
/// boundary: wrong-key stays a structured rejection (zero user calls) and a
/// consumed TakeOwned stays consumed, exactly as in the interpreter.
#[test]
fn aot_preserves_exact_contract_at_call_boundary() {
    if !native_backend_supported() {
        return;
    }

    // Wrong key under AOT.
    let create_block = compiled_import("acme::create_block", "let b = acme::create_block(7);\n");
    let take = compiled_import("acme::take", "let r = acme::ping(7); acme::take(r);\n");
    AOT_WRONG_KEY_BLOCK_PING.lock().unwrap().clear();
    AOT_NO_TAKE_CALLS.store(0, Ordering::SeqCst);
    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact(
            &create_block.name,
            1,
            create_block.schema.clone().expect("schema"),
            || Box::new(AotWrongKeyBlockPing),
        )
        .expect("register create_block");
    registry
        .register_exact_static(
            &take.name,
            take.arity,
            take.schema.clone().expect("schema"),
            aot_no_take_counted,
        )
        .expect("register take");
    let mut vm = Vm::try_new(ping_then_call_program(&create_block, &take, 1))
        .expect("test VM construction must not fail");
    registry.bind_vm_cached(&mut vm).expect("bind");
    vm.compile_aot().expect("aot compile");
    let error = vm
        .run()
        .expect_err("wrong-key TakeOwned stays rejected under AOT");
    assert_eq!(
        error.resource_error_code(),
        Some(ResourceErrorCode::ResourceKeyMismatch),
        "wrong key must stay a structured key mismatch under AOT, got: {error}"
    );
    assert_eq!(AOT_NO_TAKE_CALLS.load(Ordering::SeqCst), 0);

    // Consumed TakeOwned under AOT.
    let ping = compiled_import("acme::ping", "let r = acme::ping(7);\n");
    let take = compiled_import("acme::take", "let r = acme::ping(7); acme::take(r);\n");
    AOT_CONSUMED_PING.lock().unwrap().clear();
    AOT_TAKE_ONE_CALLS.store(0, Ordering::SeqCst);
    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact(&ping.name, 1, ping.schema.clone().expect("schema"), || {
            Box::new(AotConsumedPing)
        })
        .expect("register ping");
    registry
        .register_exact_static(
            &take.name,
            take.arity,
            take.schema.clone().expect("schema"),
            aot_take_first_arg_counted,
        )
        .expect("register take");
    let mut vm = Vm::try_new(ping_then_call_program(&ping, &take, 1))
        .expect("test VM construction must not fail");
    registry.bind_vm_cached(&mut vm).expect("bind");
    vm.compile_aot().expect("aot compile");
    let status = vm.run().expect("consumed take under AOT");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(7)]);
    let (raw, closes) = {
        let locked = AOT_CONSUMED_PING.lock().unwrap();
        (locked[0].0, locked[0].1.clone())
    };
    assert_eq!(
        vm.host_context()
            .execution_scope()
            .resources()
            .ownership(ResourceHandle::from_raw(raw).expect("valid handle")),
        Some(ResourceOwnership::Taken),
        "handle must be Taken after an AOT-run consume"
    );
    assert_eq!(closes.load(Ordering::SeqCst), 0, "taken not closed");
}

// Keep `Resource` and `ResourceAccessRequest` referenced for compile-safe
// imports even though most usage is via the frame API.
#[allow(dead_code)]
fn _type_anchors() -> (Resource<FileResource>, ResourceAccessRequest) {
    unreachable!()
}
