#![cfg(feature = "runtime")]

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use vm::compiler::{HostCallResolver, TypeSchema};
use vm::resource::{CloseProgress, HostResource, ResourceCloseReason, ResourceHandle};
use vm::{
    BuiltinFunction, CallOutcome, CallReturn, CompileSourceFileOptions, HostApiBuilder,
    HostApiCatalog, HostFunction, HostFunctionSchema, HostParamPassing, HostParamSchema,
    HostTypeSchema, ResourceTypeKey, ResourceTypeSchema, Value, Vm, VmError, VmResult, VmStatus,
    compile_source_with_flavor_and_options,
};
fn key(name: &str) -> ResourceTypeKey {
    ResourceTypeKey::new(name).expect("test resource key")
}

#[test]
fn resource_schema_is_nominal_and_preserves_its_key() {
    let schema = TypeSchema::Resource(key("sqlite.connection"));
    assert_eq!(schema.resource_key(), Some(&key("sqlite.connection")));
    assert_ne!(schema, TypeSchema::Map(Box::new(TypeSchema::Unknown)));
    assert_eq!(schema.resource_abi_value_type(), vm::ValueType::Int);
}

#[test]
fn host_resource_schema_maps_recursively_to_compiler_schema() {
    let host = HostTypeSchema::Optional(Box::new(HostTypeSchema::Array(Box::new(
        HostTypeSchema::Resource(key("io.file")),
    ))));
    let mapped = host.to_compiler_schema();
    assert_eq!(
        mapped,
        TypeSchema::Optional(Box::new(TypeSchema::Array(Box::new(TypeSchema::Resource(
            key("io.file")
        ),))))
    );
}

#[test]
fn host_resolver_preserves_nominal_key_and_passing_mode() {
    let resource_key = key("sqlite.connection");
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(
        resource_key.clone(),
        "SQLite connection",
    ));
    let open = HostFunctionSchema::with_return(
        "sqlite::open",
        vec![HostParamSchema::value("path", HostTypeSchema::String)],
        HostTypeSchema::Resource(resource_key.clone()),
    );
    let close = HostFunctionSchema::with_return(
        "sqlite::close",
        vec![HostParamSchema::with_passing(
            "connection",
            HostTypeSchema::Resource(resource_key.clone()),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::Null,
    );
    builder.function(open);
    builder.function(close);
    let catalog = builder.build().expect("valid catalog");
    let resolver = HostCallResolver::new(&catalog);

    let opened = resolver
        .resolve("sqlite::open", &[TypeSchema::String])
        .expect("open resolves");
    assert_eq!(
        opened.return_type,
        TypeSchema::Resource(resource_key.clone())
    );
    let closed = resolver
        .resolve(
            "sqlite::close",
            &[TypeSchema::Resource(resource_key.clone())],
        )
        .expect("close resolves");
    assert_eq!(closed.passing, vec![HostParamPassing::TakeOwned]);
}

#[test]
fn standard_catalog_contains_exact_resource_close_schemas() {
    let catalog = vm::standard_host_catalog();

    let io_close = catalog
        .function("io::close")
        .expect("standard catalog must contain io::close");
    assert_eq!(
        io_close.params,
        vec![HostParamSchema::with_passing(
            "handle",
            HostTypeSchema::Resource(key("io.file")),
            HostParamPassing::TakeOwned,
        )]
    );
    assert_eq!(io_close.return_type, HostTypeSchema::Bool);

    let sqlite_close = catalog
        .function("sqlite::close")
        .expect("standard catalog must contain sqlite::close");
    assert_eq!(
        sqlite_close.params,
        vec![HostParamSchema::with_passing(
            "connection",
            HostTypeSchema::Resource(key("sqlite.connection")),
            HostParamPassing::TakeOwned,
        )]
    );
    assert_eq!(sqlite_close.return_type, HostTypeSchema::Null);
}

#[test]
fn standard_catalog_resolves_open_then_close_with_nominal_ownership() {
    let catalog = vm::standard_host_catalog();
    let resolver = HostCallResolver::new(&catalog);

    let file = resolver
        .resolve("io::open", &[TypeSchema::String, TypeSchema::String])
        .expect("standard io::open resolves");
    assert_eq!(
        file.return_type,
        TypeSchema::Resource(key("io.file")),
        "io::open must produce the IO resource key"
    );
    let io_close = resolver
        .resolve("io::close", &[TypeSchema::Resource(key("io.file"))])
        .expect("standard io::close resolves");
    assert_eq!(io_close.passing, vec![HostParamPassing::TakeOwned]);
    assert_eq!(io_close.return_type, TypeSchema::Bool);

    let connection = resolver
        .resolve("sqlite::open", &[TypeSchema::Unknown])
        .expect("standard sqlite::open resolves");
    assert_eq!(
        connection.return_type,
        TypeSchema::Resource(key("sqlite.connection")),
        "sqlite::open must produce the SQLite resource key"
    );
    let sqlite_close = resolver
        .resolve(
            "sqlite::close",
            &[TypeSchema::Resource(key("sqlite.connection"))],
        )
        .expect("standard sqlite::close resolves");
    assert_eq!(sqlite_close.passing, vec![HostParamPassing::TakeOwned]);
    assert_eq!(sqlite_close.return_type, TypeSchema::Null);
}

#[test]
fn default_source_compilation_accepts_open_use_close_ownership_flow() {
    let options =
        CompileSourceFileOptions::new().with_host_api_catalog(vm::standard_host_catalog());
    let source = r#"
        use io;
        use sqlite;
        let file = io::open("Cargo.toml", "r");
        io::close(file);
        let connection = sqlite::open({});
        sqlite::close(connection);
    "#;
    compile_source_with_flavor_and_options(source, vm::SourceFlavor::RustScript, options)
        .expect("standard catalog must compile normal open/use/close ownership flow");
}

#[test]
fn resource_declaration_parser_keeps_qualified_key() {
    let result = vm::compile_source("let handle: resource<io.file> = 1;");
    let error = match result {
        Ok(_) => panic!("integer cannot initialize a resource"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("resource"));
}

#[test]
fn borrowed_resource_capture_cannot_escape_into_closure() {
    let options = CompileSourceFileOptions::new().with_host_api_catalog(closure_catalog());
    let source = r#"
        let handle = test::make_closure();
        let inspect = || test::borrow_closure(handle);
        inspect();
    "#;
    let error =
        match compile_source_with_flavor_and_options(source, vm::SourceFlavor::RustScript, options)
        {
            Ok(_) => panic!("borrowed resource capture must not escape into a closure"),
            Err(error) => error,
        };
    assert!(
        error.to_string().contains("borrow"),
        "unexpected compiler error: {error}"
    );
}

#[test]
fn resource_copy_is_rejected_for_move_only_local() {
    let options = CompileSourceFileOptions::new().with_host_api_catalog(closure_catalog());
    let source = r#"
        let handle = test::make_closure();
        let copied = handle.copy();
        copied;
    "#;
    let error =
        match compile_source_with_flavor_and_options(source, vm::SourceFlavor::RustScript, options)
        {
            Ok(_) => panic!("resource copy must be rejected"),
            Err(error) => error,
        };
    assert!(
        error.to_string().contains("copy"),
        "unexpected compiler error: {error}"
    );
}

#[test]
fn user_function_return_resource_is_owned_at_call_site() {
    let options = CompileSourceFileOptions::new().with_host_api_catalog(closure_catalog());
    let source = r#"
        fn make_resource() -> resource<pr24.closure> {
            test::make_closure();
        }
        let handle = make_resource();
        test::close_closure(handle);
        handle;
    "#;
    let error =
        match compile_source_with_flavor_and_options(source, vm::SourceFlavor::RustScript, options)
        {
            Ok(_) => panic!("a returned resource must be unavailable after TakeOwned close"),
            Err(error) => error,
        };
    assert!(
        error.to_string().contains("moved"),
        "unexpected compiler error: {error}"
    );
}

#[test]
fn closure_capture_can_transfer_resource_to_take_owned_host_call() {
    let options =
        CompileSourceFileOptions::new().with_host_api_catalog(vm::standard_host_catalog());
    let source = r#"
        use io;
        let file = io::open("Cargo.toml", "r");
        let close_file = || io::close(file);
        close_file();
    "#;
    compile_source_with_flavor_and_options(source, vm::SourceFlavor::RustScript, options)
        .expect("a closure may move its captured resource into a TakeOwned host call");
}
#[cfg(feature = "runtime")]
static BLOCK_CLOSES: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
struct BlockResource;

impl HostResource for BlockResource {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(key("pr24.block"))
    }

    fn begin_close(
        &mut self,
        _reason: ResourceCloseReason,
    ) -> vm::resource::ResourceResult<CloseProgress> {
        BLOCK_CLOSES.fetch_add(1, Ordering::SeqCst);
        Ok(CloseProgress::Ready)
    }
}

struct MakeBlockResource;

impl HostFunction for MakeBlockResource {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        let token = vm
            .host_context()
            .push_resource(BlockResource)
            .map_err(|error| VmError::HostError(error.to_string()))?;
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(
            token.handle().raw() as i64,
        ))))
    }
}

struct CloseBlockResource;

impl HostFunction for CloseBlockResource {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let Some(Value::Int(raw)) = args.first() else {
            return Err(VmError::TypeMismatch("resource handle"));
        };
        let handle = ResourceHandle::from_raw(*raw as u64)
            .map_err(|error| VmError::HostError(error.to_string()))?;
        vm.execution_scope()
            .close_resource::<BlockResource>(handle, ResourceCloseReason::Requested)
            .map_err(|error| VmError::HostError(error.to_string()))?;
        Ok(CallOutcome::Return(CallReturn::none()))
    }
}

#[cfg(feature = "runtime")]
static CLOSURE_CLOSES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "runtime")]
static CLOSURE_TEST_LOCK: Mutex<()> = Mutex::new(());
#[cfg(feature = "runtime")]
static LAST_LOCAL_RAW: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "runtime")]
static LAST_ERR_RAW: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "runtime")]
static USE_OWNED_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "runtime")]
static LATER_ARG_CALLS: AtomicUsize = AtomicUsize::new(0);
static BORROW_BOUNDARY_CALLS: AtomicUsize = AtomicUsize::new(0);
static AOTNULL_ERR_CALLS: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
struct ClosureResource;

impl HostResource for ClosureResource {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(key("pr24.closure"))
    }

    fn begin_close(
        &mut self,
        _reason: ResourceCloseReason,
    ) -> vm::resource::ResourceResult<CloseProgress> {
        CLOSURE_CLOSES.fetch_add(1, Ordering::SeqCst);
        Ok(CloseProgress::Ready)
    }
}

struct MakeClosureResource;

impl HostFunction for MakeClosureResource {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        let token = vm
            .host_context()
            .push_resource(ClosureResource)
            .map_err(|error| VmError::HostError(error.to_string()))?;
        let raw = token.handle().raw();
        LAST_LOCAL_RAW.store(raw, Ordering::SeqCst);
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(raw as i64))))
    }
}

struct CloseClosureResource;

impl HostFunction for CloseClosureResource {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let Some(Value::Int(raw)) = args.first() else {
            return Err(VmError::TypeMismatch("resource handle"));
        };
        let handle = ResourceHandle::from_raw(*raw as u64)
            .map_err(|error| VmError::HostError(error.to_string()))?;
        vm.execution_scope()
            .close_resource::<ClosureResource>(handle, ResourceCloseReason::Requested)
            .map_err(|error| VmError::HostError(error.to_string()))?;
        Ok(CallOutcome::Return(CallReturn::none()))
    }
}

fn closure_catalog() -> Arc<HostApiCatalog> {
    let mut builder = HostApiBuilder::new();
    let resource_key = key("pr24.closure");
    builder.resource(ResourceTypeSchema::new(
        resource_key.clone(),
        "closure resource",
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::make_closure",
        Vec::new(),
        HostTypeSchema::Resource(resource_key.clone()),
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::borrow_closure",
        vec![HostParamSchema::with_passing(
            "resource",
            HostTypeSchema::Resource(resource_key.clone()),
            HostParamPassing::Borrow,
        )],
        HostTypeSchema::Null,
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::close_closure",
        vec![HostParamSchema::with_passing(
            "resource",
            HostTypeSchema::Resource(resource_key),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::Null,
    ));
    Arc::new(builder.build().expect("closure catalog should build"))
}
fn block_catalog() -> Arc<HostApiCatalog> {
    let mut builder = HostApiBuilder::new();
    let resource_key = key("pr24.block");
    builder.resource(ResourceTypeSchema::new(
        resource_key.clone(),
        "block resource",
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::make_block",
        Vec::new(),
        HostTypeSchema::Resource(resource_key.clone()),
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::make_optional",
        Vec::new(),
        HostTypeSchema::Optional(Box::new(HostTypeSchema::Resource(resource_key.clone()))),
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::make_optional_map",
        Vec::new(),
        HostTypeSchema::Optional(Box::new(HostTypeSchema::Map(Box::new(
            HostTypeSchema::Resource(resource_key.clone()),
        )))),
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::close_block",
        vec![HostParamSchema::with_passing(
            "resource",
            HostTypeSchema::Resource(resource_key),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::Null,
    ));
    Arc::new(builder.build().expect("catalog should build"))
}

#[test]
fn compiled_expression_block_detaches_taken_resource_once() {
    BLOCK_CLOSES.store(0, Ordering::SeqCst);
    let options = CompileSourceFileOptions::new().with_host_api_catalog(block_catalog());
    let source = r#"
        let handle = test::make_block();
        let result = if true => {
            test::close_block(handle);
            7
        } else => {
            0
        };
        result;
    "#;
    let compiled =
        compile_source_with_flavor_and_options(source, vm::SourceFlavor::RustScript, options)
            .expect("resource block source should compile");
    let detach_call = BuiltinFunction::DetachLocal.call_index().to_le_bytes();
    assert!(
        compiled
            .program
            .code
            .windows(4)
            .any(|window| { window[0] == vm::OpCode::Call as u8 && window[1..3] == detach_call })
    );
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.bind_function("test::make_block", Box::new(MakeBlockResource));
    vm.bind_function("test::close_block", Box::new(CloseBlockResource));
    assert_eq!(
        vm.run().expect("resource block should run"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(7)]);
    assert_eq!(BLOCK_CLOSES.load(Ordering::SeqCst), 1);
    assert_eq!(vm.drop_contract_event_count(), 0);
}

#[test]
fn closure_capture_executes_take_owned_resource_once() {
    let _guard = CLOSURE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    CLOSURE_CLOSES.store(0, Ordering::SeqCst);
    let options = CompileSourceFileOptions::new().with_host_api_catalog(closure_catalog());
    let source = r#"
        let handle = test::make_closure();
        let close_handle = || test::close_closure(handle);
        close_handle();
        1;
    "#;
    let compiled =
        compile_source_with_flavor_and_options(source, vm::SourceFlavor::RustScript, options)
            .expect("closure resource source should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.bind_function("test::make_closure", Box::new(MakeClosureResource));
    vm.bind_function("test::close_closure", Box::new(CloseClosureResource));
    assert_eq!(
        vm.run().expect("closure resource should run"),
        VmStatus::Halted
    );
    assert_eq!(
        vm.stack().last(),
        Some(&Value::Int(1)),
        "closure result should remain on the value stack"
    );
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);
}

#[test]
fn named_function_capture_executes_take_owned_resource_once() {
    let _guard = CLOSURE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    CLOSURE_CLOSES.store(0, Ordering::SeqCst);
    let options = CompileSourceFileOptions::new().with_host_api_catalog(closure_catalog());
    let source = r#"
        let handle = test::make_closure();
        fn close_handle() {
            test::close_closure(handle);
        }
        close_handle();
        1;
    "#;
    let compiled =
        compile_source_with_flavor_and_options(source, vm::SourceFlavor::RustScript, options)
            .expect("named function resource source should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    vm.bind_function("test::make_closure", Box::new(MakeClosureResource));
    vm.bind_function("test::close_closure", Box::new(CloseClosureResource));
    assert_eq!(
        vm.run().expect("named function resource should run"),
        VmStatus::Halted
    );
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);
}

#[test]
fn unwrap_or_consumes_owned_optional_source_for_later_use_check() {
    let options = CompileSourceFileOptions::new().with_host_api_catalog(block_catalog());
    let source = r#"
        let maybe = test::make_optional();
        let handle = maybe.unwrap_or(test::make_block());
        test::close_block(handle);
        maybe;
    "#;
    let error =
        match compile_source_with_flavor_and_options(source, vm::SourceFlavor::RustScript, options)
        {
            Ok(_) => panic!("an optional source moved by unwrap_or cannot be used later"),
            Err(error) => error,
        };
    assert!(
        error.to_string().contains("moved"),
        "unexpected compiler error: {error}"
    );
}

#[test]
fn optional_get_then_unwrap_consumes_owned_container_for_later_use_check() {
    let options = CompileSourceFileOptions::new().with_host_api_catalog(block_catalog());
    let source = r#"
        let maybe: map<resource<pr24.block>>? = test::make_optional_map();
        let handle = maybe?.["resource"].unwrap_or(test::make_block());
        test::close_block(handle);
        maybe;
    "#;
    let error =
        match compile_source_with_flavor_and_options(source, vm::SourceFlavor::RustScript, options)
        {
            Ok(_) => panic!("an optional-get source moved by unwrap_or cannot be used later"),
            Err(error) => error,
        };
    assert!(
        error.to_string().contains("moved"),
        "unexpected compiler error: {error}"
    );
}

#[test]
fn match_consumes_owned_scrutinee_for_later_use_check() {
    let options = CompileSourceFileOptions::new().with_host_api_catalog(block_catalog());
    let source = r#"
        let maybe = test::make_optional();
        let handle = match maybe {
            Some(value) => value,
            _ => test::make_block(),
        };
        test::close_block(handle);
        maybe;
    "#;
    let error =
        match compile_source_with_flavor_and_options(source, vm::SourceFlavor::RustScript, options)
        {
            Ok(_) => panic!("a match scrutinee moved into its result cannot be used later"),
            Err(error) => error,
        };
    assert!(
        error.to_string().contains("moved"),
        "unexpected compiler error: {error}"
    );
}

#[test]
fn expression_block_rejects_later_resource_use() {
    let options = CompileSourceFileOptions::new().with_host_api_catalog(block_catalog());
    let source = r#"
        let handle = test::make_block();
        let result = if true => {
            test::close_block(handle);
            7
        } else => {
            0
        };
        handle;
    "#;
    let error =
        match compile_source_with_flavor_and_options(source, vm::SourceFlavor::RustScript, options)
        {
            Ok(_) => panic!("a moved resource cannot be used after the expression block"),
            Err(error) => error,
        };
    let rendered = error.to_string();
    assert!(
        rendered.contains("moved"),
        "unexpected compiler error: {rendered}"
    );
}

fn result_resource_catalog() -> Arc<HostApiCatalog> {
    let mut builder = HostApiBuilder::new();
    let resource_key = key("pr24.closure");
    builder.resource(ResourceTypeSchema::new(
        resource_key.clone(),
        "closure resource",
    ));
    builder.resource(ResourceTypeSchema::new(key("pr24.block"), "block resource"));
    let result_ok = HostTypeSchema::Result {
        ok: Box::new(HostTypeSchema::Resource(resource_key.clone())),
        err: Box::new(HostTypeSchema::Int),
    };
    let result_err = HostTypeSchema::Result {
        ok: Box::new(HostTypeSchema::Int),
        err: Box::new(HostTypeSchema::Resource(resource_key.clone())),
    };
    builder.function(HostFunctionSchema::with_return(
        "test::make_ok_result",
        Vec::new(),
        result_ok.clone(),
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::make_err_result",
        Vec::new(),
        result_err,
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::make_extra_result",
        Vec::new(),
        result_ok.clone(),
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::make_wrong_key_result",
        Vec::new(),
        result_ok.clone(),
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::make_wrong_gen_result",
        Vec::new(),
        result_ok.clone(),
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::make_wrong_arena_result",
        Vec::new(),
        result_ok.clone(),
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::make_bad_tag_result",
        Vec::new(),
        result_ok.clone(),
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::make_bad_payload_result",
        Vec::new(),
        result_ok,
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::make_local",
        Vec::new(),
        HostTypeSchema::Resource(resource_key.clone()),
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::close_closure",
        vec![HostParamSchema::with_passing(
            "resource",
            HostTypeSchema::Resource(resource_key.clone()),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::Null,
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::use_owned",
        vec![
            HostParamSchema::with_passing(
                "resource",
                HostTypeSchema::Resource(resource_key.clone()),
                HostParamPassing::TakeOwned,
            ),
            HostParamSchema::value("mid", HostTypeSchema::Int),
            HostParamSchema::value("later", HostTypeSchema::Int),
        ],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::use_owned_array",
        vec![
            HostParamSchema::with_passing(
                "items",
                HostTypeSchema::Array(Box::new(HostTypeSchema::Resource(resource_key.clone()))),
                HostParamPassing::TakeOwned,
            ),
            HostParamSchema::value("later", HostTypeSchema::Int),
        ],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::later_arg",
        Vec::new(),
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::touch_borrow",
        vec![HostParamSchema::with_passing(
            "resource",
            HostTypeSchema::Resource(resource_key.clone()),
            HostParamPassing::Borrow,
        )],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::touch_borrow_mut",
        vec![
            HostParamSchema::with_passing(
                "resource",
                HostTypeSchema::Resource(resource_key.clone()),
                HostParamPassing::BorrowMut,
            ),
            HostParamSchema::value("later", HostTypeSchema::Int),
        ],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::make_block",
        Vec::new(),
        HostTypeSchema::Resource(key("pr24.block")),
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::take_null",
        vec![HostParamSchema::with_passing(
            "resource",
            HostTypeSchema::Resource(resource_key),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::Optional(Box::new(HostTypeSchema::Int)),
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::make_err_int",
        Vec::new(),
        HostTypeSchema::Result {
            ok: Box::new(HostTypeSchema::Int),
            err: Box::new(HostTypeSchema::Int),
        },
    ));
    Arc::new(
        builder
            .build()
            .expect("result resource catalog should build"),
    )
}

fn result_map(ok: bool, payload: Value, extra: bool) -> Value {
    let mut entries = vec![
        (Value::string("tag"), Value::Bool(ok)),
        (Value::string("payload"), payload),
    ];
    if extra {
        entries.push((Value::string("extra"), Value::Int(1)));
    }
    Value::map(entries)
}

struct MakeOkResult;
impl HostFunction for MakeOkResult {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        let token = vm
            .host_context()
            .push_resource(ClosureResource)
            .map_err(|error| VmError::HostError(error.to_string()))?;
        let raw = token.handle().raw();
        LAST_LOCAL_RAW.store(raw, Ordering::SeqCst);
        Ok(CallOutcome::Return(CallReturn::one(result_map(
            true,
            Value::Int(raw as i64),
            false,
        ))))
    }
}

static POLEX_EVENTS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

struct MakeErrResult;
impl HostFunction for MakeErrResult {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        POLEX_EVENTS.lock().unwrap().push("err");
        let token = vm
            .host_context()
            .push_resource(ClosureResource)
            .map_err(|error| VmError::HostError(error.to_string()))?;
        let raw = token.handle().raw();
        LAST_ERR_RAW.store(raw, Ordering::SeqCst);
        Ok(CallOutcome::Return(CallReturn::one(result_map(
            false,
            Value::Int(raw as i64),
            false,
        ))))
    }
}

struct MakeExtraResult;
impl HostFunction for MakeExtraResult {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        let token = vm
            .host_context()
            .push_resource(ClosureResource)
            .map_err(|error| VmError::HostError(error.to_string()))?;
        Ok(CallOutcome::Return(CallReturn::one(result_map(
            true,
            Value::Int(token.handle().raw() as i64),
            true,
        ))))
    }
}

struct MakeWrongKeyResult;
impl HostFunction for MakeWrongKeyResult {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        let token = vm
            .host_context()
            .push_resource(BlockResource)
            .map_err(|error| VmError::HostError(error.to_string()))?;
        Ok(CallOutcome::Return(CallReturn::one(result_map(
            true,
            Value::Int(token.handle().raw() as i64),
            false,
        ))))
    }
}

struct MakeWrongArenaResult;
impl HostFunction for MakeWrongArenaResult {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        let token = vm
            .host_context()
            .push_resource(ClosureResource)
            .map_err(|error| VmError::HostError(error.to_string()))?;
        // Arena identity occupies bits 43..62 of the public handle encoding.
        let wrong_arena = token.handle().raw() ^ (1 << 43);
        Ok(CallOutcome::Return(CallReturn::one(result_map(
            true,
            Value::Int(wrong_arena as i64),
            false,
        ))))
    }
}

struct MakeWrongGenResult;
impl HostFunction for MakeWrongGenResult {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        let token = vm
            .host_context()
            .push_resource(ClosureResource)
            .map_err(|error| VmError::HostError(error.to_string()))?;
        Ok(CallOutcome::Return(CallReturn::one(result_map(
            true,
            Value::Int(token.handle().raw().wrapping_add(1) as i64),
            false,
        ))))
    }
}

struct MakeBadTagResult;
impl HostFunction for MakeBadTagResult {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        let token = vm
            .host_context()
            .push_resource(ClosureResource)
            .map_err(|error| VmError::HostError(error.to_string()))?;
        Ok(CallOutcome::Return(CallReturn::one(Value::map(vec![
            (Value::string("tag"), Value::string("ok")),
            (
                Value::string("payload"),
                Value::Int(token.handle().raw() as i64),
            ),
        ]))))
    }
}

struct MakeBadPayloadResult;
impl HostFunction for MakeBadPayloadResult {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        Ok(CallOutcome::Return(CallReturn::one(result_map(
            true,
            Value::string("not-a-resource"),
            false,
        ))))
    }
}

struct UseOwnedResource;
impl HostFunction for UseOwnedResource {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        USE_OWNED_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(1))))
    }
}

struct UseOwnedArray;
impl HostFunction for UseOwnedArray {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        USE_OWNED_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(1))))
    }
}

struct LaterArg;
impl HostFunction for LaterArg {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        LATER_ARG_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(1))))
    }
}

struct TouchBorrow;
impl HostFunction for TouchBorrow {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(1))))
    }
}

struct TouchBorrowMut;
impl HostFunction for TouchBorrowMut {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        BORROW_BOUNDARY_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(1))))
    }
}

struct TakeNull;
impl HostFunction for TakeNull {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        POLEX_EVENTS.lock().unwrap().push("close");
        CloseClosureResource.call(vm, args)?;
        Ok(CallOutcome::Return(CallReturn::one(Value::Null)))
    }
}

struct MakeErrInt;
impl HostFunction for MakeErrInt {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        AOTNULL_ERR_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(CallOutcome::Return(CallReturn::one(result_map(
            false,
            Value::Int(9),
            false,
        ))))
    }
}

fn bind_result_hosts(vm: &mut Vm) {
    vm.bind_function("test::make_ok_result", Box::new(MakeOkResult));
    vm.bind_function("test::make_err_result", Box::new(MakeErrResult));
    vm.bind_function("test::make_extra_result", Box::new(MakeExtraResult));
    vm.bind_function("test::make_wrong_key_result", Box::new(MakeWrongKeyResult));
    vm.bind_function("test::make_wrong_gen_result", Box::new(MakeWrongGenResult));
    vm.bind_function(
        "test::make_wrong_arena_result",
        Box::new(MakeWrongArenaResult),
    );
    vm.bind_function("test::make_bad_tag_result", Box::new(MakeBadTagResult));
    vm.bind_function(
        "test::make_bad_payload_result",
        Box::new(MakeBadPayloadResult),
    );
    vm.bind_function("test::make_local", Box::new(MakeClosureResource));
    vm.bind_function("test::close_closure", Box::new(CloseClosureResource));
    vm.bind_function("test::use_owned", Box::new(UseOwnedResource));
    vm.bind_function("test::use_owned_array", Box::new(UseOwnedArray));
    vm.bind_function("test::later_arg", Box::new(LaterArg));
    vm.bind_function("test::touch_borrow", Box::new(TouchBorrow));
    vm.bind_function("test::touch_borrow_mut", Box::new(TouchBorrowMut));
    vm.bind_function("test::make_block", Box::new(MakeBlockResource));
    vm.bind_function("test::make_err_int", Box::new(MakeErrInt));
    vm.bind_function("test::take_null", Box::new(TakeNull));
}

#[test]
fn result_match_transfers_resource_after_nonconsuming_predicate() {
    let _guard = CLOSURE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    CLOSURE_CLOSES.store(0, Ordering::SeqCst);
    let options = CompileSourceFileOptions::new().with_host_api_catalog(result_resource_catalog());
    let source = r#"
        fn close_one(handle: resource<pr24.closure>) -> int {
            test::close_closure(handle);
            1
        }
        let value = test::make_ok_result();
        let flag = value.is_ok();
        let observed = match value {
            Result::Ok(handle) => close_one(handle),
            Result::Err(e) => 0,
        };
        let result = if flag => { observed } else => { 0 };
        result;
    "#;
    let compiled =
        compile_source_with_flavor_and_options(source, vm::SourceFlavor::RustScript, options)
            .expect("predicate must not consume the Result resource");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    bind_result_hosts(&mut vm);
    assert_eq!(
        vm.run().expect("Result resource match should run"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack().last().cloned(), Some(Value::Int(1)));
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);
}

#[test]
fn result_match_rejects_resource_reuse_and_keeps_err_branch_owned() {
    let options = CompileSourceFileOptions::new().with_host_api_catalog(result_resource_catalog());
    let reuse = r#"
        fn close_twice(handle: resource<pr24.closure>) -> int {
            test::close_closure(handle);
            test::close_closure(handle);
            1
        }
        let value = test::make_ok_result();
        let observed = match value {
            Result::Ok(handle) => close_twice(handle),
            Result::Err(e) => 0,
        };
        observed;
    "#;
    let error = match compile_source_with_flavor_and_options(
        reuse,
        vm::SourceFlavor::RustScript,
        options,
    ) {
        Ok(_) => panic!("consumed Result resource must not be reused"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("moved"), "{error}");

    let _guard = CLOSURE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    CLOSURE_CLOSES.store(0, Ordering::SeqCst);
    let options = CompileSourceFileOptions::new().with_host_api_catalog(result_resource_catalog());
    let source = r#"
        fn close_one(handle: resource<pr24.closure>) -> int {
            test::close_closure(handle);
            1
        }
        let value = test::make_err_result();
        let observed = match value {
            Result::Ok(v) => 0,
            Result::Err(handle) => close_one(handle),
        };
        observed;
    "#;
    let compiled =
        compile_source_with_flavor_and_options(source, vm::SourceFlavor::RustScript, options)
            .expect("Err resource should transfer through the selected arm");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    bind_result_hosts(&mut vm);
    assert_eq!(
        vm.run().expect("Err resource match should run"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack().last().cloned(), Some(Value::Int(1)));
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);
}

#[test]
fn result_host_return_rejects_extra_field_wrong_key_and_wrong_generation() {
    let _guard = CLOSURE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let options = CompileSourceFileOptions::new().with_host_api_catalog(result_resource_catalog());
    let source = "test::make_extra_result();";
    let compiled = compile_source_with_flavor_and_options(
        source,
        vm::SourceFlavor::RustScript,
        options.clone(),
    )
    .expect("extra-field call should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    bind_result_hosts(&mut vm);
    assert!(vm.run().is_err(), "extra Result field must be rejected");

    BLOCK_CLOSES.store(0, Ordering::SeqCst);
    CLOSURE_CLOSES.store(0, Ordering::SeqCst);
    let options = CompileSourceFileOptions::new().with_host_api_catalog(result_resource_catalog());
    let source = "test::make_wrong_key_result();";
    let compiled =
        compile_source_with_flavor_and_options(source, vm::SourceFlavor::RustScript, options)
            .expect("wrong-key call should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    bind_result_hosts(&mut vm);
    assert!(vm.run().is_err(), "wrong resource key must be rejected");
    drop(vm);
    assert_eq!(BLOCK_CLOSES.load(Ordering::SeqCst), 1);
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 0);

    CLOSURE_CLOSES.store(0, Ordering::SeqCst);
    let options = CompileSourceFileOptions::new().with_host_api_catalog(result_resource_catalog());
    let source = "test::make_wrong_gen_result();";
    let compiled =
        compile_source_with_flavor_and_options(source, vm::SourceFlavor::RustScript, options)
            .expect("wrong-generation call should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    bind_result_hosts(&mut vm);
    assert!(
        vm.run().is_err(),
        "wrong resource generation must be rejected"
    );
    drop(vm);
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);

    CLOSURE_CLOSES.store(0, Ordering::SeqCst);
    let options = CompileSourceFileOptions::new().with_host_api_catalog(result_resource_catalog());
    let source = "test::make_wrong_arena_result();";
    let compiled =
        compile_source_with_flavor_and_options(source, vm::SourceFlavor::RustScript, options)
            .expect("wrong-arena call should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    bind_result_hosts(&mut vm);
    assert!(vm.run().is_err(), "wrong resource arena must be rejected");
    drop(vm);
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);

    CLOSURE_CLOSES.store(0, Ordering::SeqCst);
    let options = CompileSourceFileOptions::new().with_host_api_catalog(result_resource_catalog());
    let source = "test::make_bad_tag_result();";
    let compiled =
        compile_source_with_flavor_and_options(source, vm::SourceFlavor::RustScript, options)
            .expect("bad-tag call should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    bind_result_hosts(&mut vm);
    assert!(vm.run().is_err(), "non-bool Result tag must be rejected");
    drop(vm);
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);

    CLOSURE_CLOSES.store(0, Ordering::SeqCst);
    let options = CompileSourceFileOptions::new().with_host_api_catalog(result_resource_catalog());
    let source = "test::make_bad_payload_result();";
    let compiled =
        compile_source_with_flavor_and_options(source, vm::SourceFlavor::RustScript, options)
            .expect("bad-payload call should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    bind_result_hosts(&mut vm);
    assert!(
        vm.run().is_err(),
        "non-resource Result payload must be rejected"
    );
    drop(vm);
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 0);
}

#[test]
fn result_try_cleans_local_resource_and_returns_err_resource_once() {
    let _guard = CLOSURE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    CLOSURE_CLOSES.store(0, Ordering::SeqCst);
    let options = CompileSourceFileOptions::new().with_host_api_catalog(result_resource_catalog());
    let source = r#"
        fn close_err(handle: resource<pr24.closure>) -> int {
            test::close_closure(handle);
            1
        }
        fn relay() -> Result<int, resource<pr24.closure>> {
            let local: resource<pr24.closure> = test::make_local();
            let value: int = test::make_err_result()?;
            Result::Ok(value)
        }
        let moved = match relay() {
            Result::Ok(v) => v,
            Result::Err(handle) => close_err(handle),
        };
        moved;
    "#;
    let compiled =
        compile_source_with_flavor_and_options(source, vm::SourceFlavor::RustScript, options)
            .expect("Err resource propagation should compile");
    let mut vm = Vm::new(compiled.program.with_local_count(compiled.locals));
    bind_result_hosts(&mut vm);
    assert_eq!(
        vm.run().expect("Err resource propagation should run"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack().last().cloned(), Some(Value::Int(1)));
    drop(vm);
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 2);
}

// PO-LEX uses the public RSS compiler and real resource table. All observations
// are made before VM teardown, while the original Err payload remains live.
fn assert_polex_err(body: &str, closure_closes: usize, block_closes: usize) {
    assert_polex_err_backends(body, closure_closes, block_closes, &[0, 1, 2]);
}

fn assert_polex_err_backends(
    body: &str,
    closure_closes: usize,
    block_closes: usize,
    backends: &[u8],
) {
    let source = format!(
        "fn relay() -> Result<int, resource<pr24.closure>> {{
            let owned: resource<pr24.closure> = test::make_local();
            let sibling: resource<pr24.block> = test::make_block();
            {body}
            Result::Ok(0)
        }}
        let carried = relay(); carried;"
    );
    for &backend in backends {
        eprintln!("POLEX backend={backend} body={body}");
        reset_exit_observers();
        let mut vm = run_result_source_backend(&source, backend);
        let err_raw = err_payload_raw(vm.stack().last().expect("guest Err")) as u64;
        assert_eq!(err_raw, LAST_ERR_RAW.load(Ordering::SeqCst));
        assert!(closure_is_open(&mut vm, err_raw));
        assert!(vm.execution_scope().is_active());
        assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), closure_closes);
        assert_eq!(BLOCK_CLOSES.load(Ordering::SeqCst), block_closes);
        assert_eq!(vm.execution_scope().resources().len(), 1);
        drop(vm);
        assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), closure_closes + 1);
        assert_eq!(BLOCK_CLOSES.load(Ordering::SeqCst), block_closes);
    }
}

#[test]
fn observation_statement_if_keeps_owner_for_explicit_close() {
    let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let source = "fn relay() -> int { let owned: resource<pr24.closure> = test::make_local(); let mut observed = 0; if owned == null { observed = 99; } else { observed = 1; } test::close_closure(owned); observed } relay();";
    for backend in [0, 1] {
        reset_exit_observers();
        let mut vm = run_result_source_backend(source, backend);
        assert_eq!(vm.stack().last(), Some(&Value::Int(1)));
        assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);
        assert!(!closure_is_open(
            &mut vm,
            LAST_LOCAL_RAW.load(Ordering::SeqCst)
        ));
        assert!(vm.execution_scope().resources().is_empty());
        assert!(vm.execution_scope().is_active());
        drop(vm);
        assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn observation_while_keeps_owner_across_multiple_condition_checks() {
    let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let source = "fn relay() -> int { let owned: resource<pr24.closure> = test::make_local(); let mut n = 0; while owned != null && n < 2 { n = n + 1; } test::close_closure(owned); n } relay();";
    for backend in [0, 1] {
        reset_exit_observers();
        let mut vm = run_result_source_backend(source, backend);
        assert_eq!(vm.stack().last(), Some(&Value::Int(2)));
        assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);
        assert!(!closure_is_open(
            &mut vm,
            LAST_LOCAL_RAW.load(Ordering::SeqCst)
        ));
        assert!(vm.execution_scope().resources().is_empty());
        assert!(vm.execution_scope().is_active());
        drop(vm);
        assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn observation_expression_if_and_nested_logical_keep_err_cleanup_owner() {
    let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for body in [
        "let observed = if owned == null => { 99 } else => { 1 }; let v = test::make_err_result()?;",
        "if (owned != null && true) || owned == null { test::later_arg(); } let v = test::make_err_result()?;",
        "for i in 0..(if owned != null => { 2 } else => { 0 }) { test::later_arg(); } let v = test::make_err_result()?;",
    ] {
        // Resource-vs-Null Ceq remains outside the admitted AOT instruction set.
        assert_polex_err_backends(body, 1, 1, &[0, 1]);
    }
}

#[test]
fn polex_reached_and_rhs_uses_post_lhs_owner_state() {
    let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_exit_observers();
    assert_polex_err(
        "let flag = test::close_closure(owned) == null && test::make_err_result()? == 9;",
        1,
        1,
    );
}

#[test]
fn polex_statement_if_condition_closes_sibling_before_err() {
    let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_exit_observers();
    assert_polex_err("if test::make_err_result()? == 9 { 1; } else { 0; }", 1, 1);
}

#[test]
fn polex_while_condition_closes_sibling_before_err() {
    let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_exit_observers();
    assert_polex_err("while test::make_err_result()? == 9 { break; }", 1, 1);
}

#[test]
fn polex_for_condition_closes_sibling_before_err() {
    let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_exit_observers();
    assert_polex_err("for i in 0..test::make_err_result()? { break; }", 1, 1);
}

macro_rules! polex_position_test {
    ($name:ident, $body:literal) => {
        #[test]
        fn $name() {
            let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            reset_exit_observers();
            assert_polex_err($body, 1, 1);
        }
    };
}

polex_position_test!(
    polex_reached_or_rhs,
    "let flag = !(test::close_closure(owned) == null) || test::make_err_result()? == 9;"
);
polex_position_test!(
    polex_eager_add_rhs,
    "fn take(h: resource<pr24.closure>) -> int { test::close_closure(h); 3 } let x = take(owned) + test::make_err_result()?;"
);
polex_position_test!(
    polex_nested_expression_if,
    "let x = 1 + (if test::close_closure(owned) == null => { test::make_err_result()? } else => { 0 });"
);
#[test]
fn polex_unwrap_fallback() {
    let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    assert_polex_err_backends(
        "let x = test::take_null(owned).unwrap_or(test::make_err_result()?);",
        1,
        1,
        &[0, 1, 2],
    );
    assert_eq!(*POLEX_EVENTS.lock().unwrap(), vec!["close", "err"]);
}

#[test]
fn polex_aot_optional_null_baseline_probe() {
    let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_exit_observers();
    let mut vm = run_result_source_backend(
        "fn absent() -> int? { null } fn relay() -> Result<int,int> { let x = absent().unwrap_or(test::make_err_int()?); Result::Ok(0) } let carried = relay(); carried;",
        2,
    );
    assert_eq!(err_payload_raw(vm.stack().last().unwrap()), 9);
    assert!(vm.execution_scope().is_active());
    assert_eq!(AOTNULL_ERR_CALLS.load(Ordering::SeqCst), 1);
    assert!(vm.aot_exec_count() > 0);
    assert!(vm.dump_aot_info().contains("lowering=ssa"));
    println!(
        "AOTNULL exact scalar fallback_calls=1\n{}\n{}",
        vm::disassemble_program(vm.program()),
        vm.dump_aot_info()
    );
}

#[test]
fn aotnull_exact_scalar_interpreter_and_decoded_control() {
    let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for backend in [0, 1] {
        reset_exit_observers();
        let mut vm = run_result_source_backend(
            "fn absent() -> int? { null } fn relay() -> Result<int,int> { let x = absent().unwrap_or(test::make_err_int()?); Result::Ok(0) } let carried = relay(); carried;",
            backend,
        );
        assert_eq!(err_payload_raw(vm.stack().last().unwrap()), 9);
        assert!(vm.execution_scope().is_active());
    }
}

#[test]
fn aotnull_nullable_payloads_and_fallback_effects() {
    let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for (ty, expression, fallback, expected) in [
        ("int", "null", "test::later_arg()", Value::Int(1)),
        ("int", "0", "test::later_arg()", Value::Int(0)),
        ("int", "7", "test::later_arg()", Value::Int(7)),
        ("bool", "null", "test::later_arg() == 1", Value::Bool(true)),
        (
            "bool",
            "false",
            "test::later_arg() == 1",
            Value::Bool(false),
        ),
        ("bool", "true", "test::later_arg() == 1", Value::Bool(true)),
        (
            "float",
            "null",
            "if test::later_arg() == 1 => { 1.5 } else => { 2.5 }",
            Value::Float(1.5),
        ),
        (
            "float",
            "0.0",
            "if test::later_arg() == 1 => { 1.5 } else => { 2.5 }",
            Value::Float(0.0),
        ),
        (
            "float",
            "2.5",
            "if test::later_arg() == 1 => { 1.5 } else => { 2.5 }",
            Value::Float(2.5),
        ),
    ] {
        for backend in [0, 1, 2] {
            for source in [
                format!("fn pick() -> {ty}? {{ {expression} }} pick().unwrap_or({fallback});"),
                format!(
                    "fn relay(x:{ty}?) -> {ty} {{ x.unwrap_or({fallback}) }} relay({expression});"
                ),
                format!(
                    "fn pick() -> {ty}? {{ {expression} }} let f = pick; f().unwrap_or({fallback});"
                ),
                format!(
                    "let f: fn() -> {ty}? = || if {} => {{ null }} else => {{ {} }}; let value: {ty}? = f(); value.unwrap_or({fallback});",
                    expression == "null",
                    if expression == "null" {
                        match ty {
                            "int" => "0",
                            "bool" => "false",
                            "float" => "0.0",
                            _ => unreachable!(),
                        }
                    } else {
                        expression
                    }
                ),
            ] {
                reset_exit_observers();
                let vm = run_result_source_backend(&source, backend);
                assert_eq!(
                    vm.stack().last(),
                    Some(&expected),
                    "backend={backend} {source}"
                );
                assert_eq!(
                    LATER_ARG_CALLS.load(Ordering::SeqCst),
                    usize::from(expression == "null"),
                    "backend={backend} {source}"
                );
                if backend == 2 {
                    assert!(vm.dump_aot_info().contains("lowering=ssa"));
                }
            }
        }
    }
}

#[test]
fn aotnull_fatal_fallback_is_not_guest_err() {
    let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    struct FatalFallback;
    impl HostFunction for FatalFallback {
        fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
            AOTNULL_ERR_CALLS.fetch_add(1, Ordering::SeqCst);
            Err(vm::VmError::HostError("aotnull fatal fallback".into()))
        }
    }
    for expression in ["null", "0", "7"] {
        for backend in [0, 1, 2] {
            reset_exit_observers();
            let source = format!(
                "fn pick() -> int? {{ {expression} }} fn relay() -> Result<int,int> {{ let x = pick().unwrap_or(test::make_err_int()?); Result::Ok(x) }} relay();"
            );
            let options =
                CompileSourceFileOptions::new().with_host_api_catalog(result_resource_catalog());
            let compiled = compile_source_with_flavor_and_options(
                &source,
                vm::SourceFlavor::RustScript,
                options,
            )
            .unwrap();
            let program = compiled.program.with_local_count(compiled.locals);
            let program = if backend == 1 {
                vm::decode_program(&vm::encode_program(&program).unwrap()).unwrap()
            } else {
                program
            };
            let mut vm = Vm::new(program);
            vm.set_fuel(100_000);
            bind_result_hosts(&mut vm);
            vm.bind_function("test::make_err_int", Box::new(FatalFallback));
            if backend == 2 {
                vm.compile_aot().unwrap();
            }
            let outcome = vm.run();
            if expression == "null" {
                assert!(
                    matches!(outcome, Err(vm::VmError::HostError(ref message)) if message == "aotnull fatal fallback"),
                    "{outcome:?}"
                );
                assert_eq!(AOTNULL_ERR_CALLS.load(Ordering::SeqCst), 1);
            } else {
                assert_eq!(outcome.unwrap(), VmStatus::Halted);
                assert_eq!(
                    vm.stack().last(),
                    Some(&result_map(
                        true,
                        Value::Int(expression.parse().unwrap()),
                        false
                    ))
                );
                assert_eq!(AOTNULL_ERR_CALLS.load(Ordering::SeqCst), 0);
            }
            if backend == 2 {
                assert!(vm.aot_exec_count() > 0);
            }
        }
    }
}

#[test]
fn aotnull_loop_jit_and_aot_fallback_effect_parity() {
    let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let source = "fn pick(i:int) -> int? { if i < 50 => { null } else => { 0 } } fn relay() -> int { let mut total = 0; for i in 0..100 { total = total + pick(i).unwrap_or(test::later_arg()); } total } relay();";
    for backend in [0, 1, 2, 4] {
        reset_exit_observers();
        let vm = run_result_source_backend(source, backend);
        assert_eq!(vm.stack().last(), Some(&Value::Int(50)));
        assert_eq!(LATER_ARG_CALLS.load(Ordering::SeqCst), 50);
        if backend == 4 {
            let dump = vm.dump_jit_info();
            println!(
                "AOTNULL trace\n{}",
                dump.lines()
                    .filter(|line| !line.trim_start().starts_with("code:"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            assert!(vm.jit_native_exec_count() > 0);
            assert!(vm.jit_snapshot().traces.iter().any(|trace| {
                let ssa = trace.ssa_text();
                trace.executions > 0
                    && ssa.contains("type_of")
                    && ssa.contains("String(\"null\")")
                    && ssa.contains("branch_bool")
            }));
        }
    }
}

#[test]
fn polex_optional_index() {
    let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    // AOT does not support this Optional index's Get(String) instruction.
    assert_polex_err_backends(
        "let arr: array<int>? = [1]; let x = arr?.[(if test::close_closure(owned) == null => { test::make_err_result()? } else => { 0 })];",
        1,
        1,
        &[0, 1],
    );
}

polex_position_test!(
    polex_for_init_try,
    "for i in test::make_err_result()? .. 2 { break; }"
);
polex_position_test!(
    polex_expression_if_condition,
    "let x = if test::make_err_result()? == 9 => { 1 } else => { 0 };"
);
#[test]
fn polex_nested_match() {
    let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    // Optional-pattern Ceq is outside AOT's admitted instruction set.
    assert_polex_err_backends(
        "let x = 1 + (match test::take_null(owned) { Some(v) => v, _ => test::make_err_result()?, });",
        1,
        1,
        &[0, 1],
    );
}
polex_position_test!(
    polex_nested_result_match,
    "fn take(h: resource<pr24.closure>) -> Result<int,int> { test::close_closure(h); Result::Err(3) } let x = 1 + (match take(owned) { Result::Ok(v) => v, Result::Err(e) => test::make_err_result()?, });"
);
polex_position_test!(
    polex_aggregate_sibling,
    "fn take(h: resource<pr24.closure>) -> int { test::close_closure(h); 3 } let x = [take(owned), test::make_err_result()?];"
);
polex_position_test!(
    polex_call_sibling,
    "fn take(h: resource<pr24.closure>) -> int { test::close_closure(h); 3 } fn sum(a:int,b:int)->int { a+b } let x = sum(take(owned), test::make_err_result()?);"
);

#[test]
fn polex_eager_binary_runs_once_in_left_to_right_order() {
    let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    assert_polex_err(
        "fn take(h: resource<pr24.closure>) -> int { test::close_closure(h); 3 } let x = take(owned) - test::make_err_result()?;",
        1,
        1,
    );
    assert_eq!(*POLEX_EVENTS.lock().unwrap(), vec!["close", "err"]);
}

#[test]
fn polex_reached_and_skipped_try_both_operators() {
    let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for op in ["&&", "||"] {
        for flag in [false, true] {
            assert_polex_err(
                &format!(
                    "let x = ((test::close_closure(owned) == null) == {flag}) {op} (test::make_err_result()? == 9); test::later_arg(); let v = test::make_err_result()?;"
                ),
                1,
                1,
            );
            let reached = if op == "&&" { flag } else { !flag };
            assert_eq!(
                LATER_ARG_CALLS.load(Ordering::SeqCst),
                usize::from(!reached)
            );
            assert_eq!(*POLEX_EVENTS.lock().unwrap(), vec!["close", "err"]);
        }
    }
}

#[test]
fn polex_finite_loop_break_continue_and_post_controls() {
    let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for body in [
        "while true { let closed = test::close_closure(owned); break; } let v = test::make_err_result()?;",
        "while false { let closed = test::close_closure(owned); break; } let v = test::make_err_result()?;",
        "for i in 0..2 { let closed = test::close_closure(owned); break; } let v = test::make_err_result()?;",
        "for i in 0..0 { let closed = test::close_closure(owned); break; } let v = test::make_err_result()?;",
        "for i in 0..2 { if i == 0 { continue; } let v = test::make_err_result()?; }",
    ] {
        assert_polex_err(body, 1, 1);
    }
}

#[test]
fn polex_optional_fallback_skipped_owner_is_not_definitely_moved() {
    let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for flag in [false, true] {
        assert_polex_err(
            &format!(
                "let maybe: int? = if {flag} => {{ 1 }} else => {{ null }}; let x = maybe.unwrap_or(if test::close_closure(owned) == null => {{ 2 }} else => {{ 3 }}); let v = test::make_err_result()?;"
            ),
            1,
            1,
        );
        let expected = if flag {
            vec!["err"]
        } else {
            vec!["close", "err"]
        };
        assert_eq!(*POLEX_EVENTS.lock().unwrap(), expected);
    }
}

#[test]
fn polex_short_circuit_skipped_rhs_retains_owner_for_later_try() {
    let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for op in ["&&", "||"] {
        for flag in [false, true] {
            reset_exit_observers();
            assert_polex_err(
                &format!(
                    "let flag = {flag} {op} (test::close_closure(owned) == null); let value = test::make_err_result()?;"
                ),
                1,
                1,
            );
            let reached = if op == "&&" { flag } else { !flag };
            let expected = if reached {
                vec!["close", "err"]
            } else {
                vec!["err"]
            };
            assert_eq!(*POLEX_EVENTS.lock().unwrap(), expected);
        }
    }
}

#[test]
fn polex_loop_backedge_fresh_owner_is_in_condition_cleanup() {
    let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for body in [
        "let mut i = 0; while if i == 0 => { true } else => { test::make_err_result()? == 9 } { let inner: resource<pr24.closure> = test::make_local(); i = i + 1; continue; }",
        "let mut n = 0; for i in 0..(if n == 0 => { 2 } else => { test::make_err_result()? }) { let inner: resource<pr24.closure> = test::make_local(); n = n + 1; continue; }",
    ] {
        reset_exit_observers();
        assert_polex_err(body, 2, 1);
    }
}

#[test]
fn polex_scalar_error_conditions_close_before_vm_drop() {
    let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    for body in [
        "let x = test::close_closure(owned) == null && test::make_err_int()? == 9;",
        "if test::make_err_int()? == 9 { 1; } else { 0; }",
        "while test::make_err_int()? == 9 { break; }",
        "for i in 0..test::make_err_int()? { break; }",
    ] {
        let source = format!(
            "fn relay() -> Result<int,int> {{ let owned: resource<pr24.closure> = test::make_local(); let sibling: resource<pr24.block> = test::make_block(); {body} Result::Ok(0) }} let carried = relay(); carried;"
        );
        for backend in 0..3 {
            reset_exit_observers();
            let mut vm = run_result_source_backend(&source, backend);
            assert_eq!(err_payload_raw(vm.stack().last().unwrap()), 9);
            assert!(vm.execution_scope().is_active());
            assert_eq!(vm.execution_scope().resources().len(), 0);
            assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);
            assert_eq!(BLOCK_CLOSES.load(Ordering::SeqCst), 1);
            drop(vm);
            assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);
            assert_eq!(BLOCK_CLOSES.load(Ordering::SeqCst), 1);
        }
    }
}

fn reset_exit_observers() {
    AOTNULL_ERR_CALLS.store(0, Ordering::SeqCst);
    POLEX_EVENTS.lock().unwrap().clear();
    CLOSURE_CLOSES.store(0, Ordering::SeqCst);
    BLOCK_CLOSES.store(0, Ordering::SeqCst);
    LAST_LOCAL_RAW.store(0, Ordering::SeqCst);
    LAST_ERR_RAW.store(0, Ordering::SeqCst);
    USE_OWNED_CALLS.store(0, Ordering::SeqCst);
    LATER_ARG_CALLS.store(0, Ordering::SeqCst);
    BORROW_BOUNDARY_CALLS.store(0, Ordering::SeqCst);
}

fn closure_is_open(vm: &mut Vm, raw: u64) -> bool {
    let Ok(handle) = ResourceHandle::from_raw(raw) else {
        return false;
    };
    vm.execution_scope()
        .resources()
        .typed::<ClosureResource>(handle)
        .is_ok()
}

fn err_payload_raw(value: &Value) -> i64 {
    let Value::Map(map) = value else {
        panic!("expected returned Result map before VM drop, got {value:?}");
    };
    assert_eq!(
        map.get(&Value::string("tag")),
        Some(&Value::Bool(false)),
        "Err exit must return the original Err carrier, got {value:?}"
    );
    match map.get(&Value::string("payload")) {
        Some(Value::Int(raw)) => *raw,
        other => panic!("expected resource Err payload, got {other:?}"),
    }
}

fn assert_local_closed_and_err_open_before_drop(vm: &mut Vm, local_raw: u64, err_raw: u64) {
    let local_open = closure_is_open(vm, local_raw);
    let err_open = closure_is_open(vm, err_raw);
    let closes = CLOSURE_CLOSES.load(Ordering::SeqCst);
    let live = vm.execution_scope().resources().len();
    assert!(
        !local_open && err_open && closes == 1,
        "before VM drop the owned local must be closed once and the returned Err resource must still be open and usable: closes={closes} local_open={local_open} err_open={err_open} live={live} local={local_raw} err={err_raw}"
    );
    let err_handle = ResourceHandle::from_raw(err_raw).expect("returned Err handle");
    vm.execution_scope()
        .close_resource::<ClosureResource>(err_handle, ResourceCloseReason::Requested)
        .expect("caller should close the still-open Err resource once");
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 2);
    assert!(!closure_is_open(vm, err_raw));
}

fn run_result_source(source: &str) -> Vm {
    run_result_source_backend(source, 3)
}

fn run_result_source_backend(source: &str, backend: u8) -> Vm {
    let options = CompileSourceFileOptions::new().with_host_api_catalog(result_resource_catalog());
    let compiled =
        compile_source_with_flavor_and_options(source, vm::SourceFlavor::RustScript, options)
            .unwrap_or_else(|error| {
                panic!("resource try source should compile: {error:?}\n{source}")
            });
    let program = compiled.program.with_local_count(compiled.locals);
    let program = if backend == 1 {
        vm::decode_program(&vm::encode_program(&program).unwrap()).unwrap()
    } else {
        program
    };
    let mut vm = if backend == 4 {
        Vm::new_with_jit_config(
            program,
            vm::JitConfig {
                enabled: true,
                hot_loop_threshold: 1,
                max_trace_len: 512,
            },
        )
    } else {
        Vm::new(program)
    };
    vm.set_fuel(100_000);
    bind_result_hosts(&mut vm);
    if backend < 3 {
        // The legacy fixture's Null host returns zero values. These tests bind
        // its explicit Null value so a let consumes exactly one value and the
        // AOT branch shapes match the host signature.
        struct ExplicitClose;
        impl HostFunction for ExplicitClose {
            fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
                POLEX_EVENTS.lock().unwrap().push("close");
                CloseClosureResource.call(vm, args)?;
                Ok(CallOutcome::Return(CallReturn::one(Value::Null)))
            }
        }
        vm.bind_function("test::close_closure", Box::new(ExplicitClose));
    }
    if backend == 2 {
        vm.compile_aot().expect("A03 AOT compile");
    }
    assert_eq!(
        vm.run().unwrap_or_else(|error| panic!(
            "resource try should run (backend={backend}): {error:?}"
        )),
        VmStatus::Halted
    );
    if backend == 2 {
        assert!(vm.aot_exec_count() > 0);
    }
    vm
}

#[test]
fn result_try_closes_live_local_before_vm_drop_and_keeps_err_open() {
    let _guard = CLOSURE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_exit_observers();
    let source = r#"
        fn relay() -> Result<int, resource<pr24.closure>> {
            let local: resource<pr24.closure> = test::make_local();
            let value: int = test::make_err_result()?;
            test::close_closure(local);
            Result::Ok(value)
        }
        let carried = relay();
        carried;
    "#;
    let mut vm = run_result_source(source);
    let err_raw = err_payload_raw(vm.stack().last().expect("returned Result"));
    let local_raw = LAST_LOCAL_RAW.load(Ordering::SeqCst);
    assert_eq!(err_raw as u64, LAST_ERR_RAW.load(Ordering::SeqCst));
    assert_ne!(local_raw, 0);
    assert_local_closed_and_err_open_before_drop(&mut vm, local_raw, err_raw as u64);
    drop(vm);
    assert_eq!(
        CLOSURE_CLOSES.load(Ordering::SeqCst),
        2,
        "VM drop must not close the already closed local or Err resource again"
    );
}

#[test]
fn result_try_decoded_program_closes_live_local_before_vm_drop() {
    let _guard = CLOSURE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_exit_observers();
    let source = r#"
        fn relay() -> Result<int, resource<pr24.closure>> {
            let local: resource<pr24.closure> = test::make_local();
            let value: int = test::make_err_result()?;
            test::close_closure(local);
            Result::Ok(value)
        }
        let carried = relay();
        carried;
    "#;
    let options = CompileSourceFileOptions::new().with_host_api_catalog(result_resource_catalog());
    let compiled =
        compile_source_with_flavor_and_options(source, vm::SourceFlavor::RustScript, options)
            .expect("resource try source should compile");
    let bytes = vm::encode_program(&compiled.program.with_local_count(compiled.locals))
        .expect("compiled resource try program should encode");
    let decoded = vm::decode_program(&bytes).expect("compiled resource try program should decode");
    let mut vm = Vm::new(decoded.with_local_count(compiled.locals));
    vm.set_fuel(100_000);
    bind_result_hosts(&mut vm);
    assert_eq!(
        vm.run().unwrap_or_else(|error| {
            panic!("decoded resource try should run on a fresh VM: {error:?}")
        }),
        VmStatus::Halted
    );
    let err_raw = err_payload_raw(vm.stack().last().expect("returned Result"));
    let local_raw = LAST_LOCAL_RAW.load(Ordering::SeqCst);
    assert_eq!(err_raw as u64, LAST_ERR_RAW.load(Ordering::SeqCst));
    assert_ne!(local_raw, 0);
    assert_local_closed_and_err_open_before_drop(&mut vm, local_raw, err_raw as u64);
    drop(vm);
    assert_eq!(
        CLOSURE_CLOSES.load(Ordering::SeqCst),
        2,
        "VM drop must not close the already closed local or Err resource again"
    );
}

#[test]
fn result_try_aot_artifact_closes_live_local_before_vm_drop() {
    let _guard = CLOSURE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_exit_observers();
    let source = r#"
        fn relay() -> Result<int, resource<pr24.closure>> {
            let local: resource<pr24.closure> = test::make_local();
            let value: int = test::make_err_result()?;
            test::close_closure(local);
            Result::Ok(value)
        }
        let carried = relay();
        carried;
    "#;
    let options = CompileSourceFileOptions::new().with_host_api_catalog(result_resource_catalog());
    let compiled =
        compile_source_with_flavor_and_options(source, vm::SourceFlavor::RustScript, options)
            .expect("resource try source should compile");
    let jit = vm::JitConfig {
        enabled: false,
        hot_loop_threshold: 1,
        max_trace_len: 512,
    };
    let mut producer = Vm::new_with_jit_config(
        compiled.program.with_local_count(compiled.locals),
        jit.clone(),
    );
    producer.set_fuel(100_000);
    bind_result_hosts(&mut producer);
    let artifact = producer
        .encode_aot_artifact()
        .expect("AOT artifact should embed the current v13 cleanup section");
    drop(producer);
    reset_exit_observers();
    let mut vm = Vm::new_from_aot_artifact_with_jit_config(&artifact, jit)
        .expect("fresh VM should load the artifact program and SSA code");
    vm.set_fuel(100_000);
    bind_result_hosts(&mut vm);
    assert_eq!(
        vm.run().unwrap_or_else(|error| {
            panic!("artifact AOT resource try should run on a fresh VM: {error:?}")
        }),
        VmStatus::Halted
    );
    assert!(
        vm.aot_exec_count() > 0,
        "SSA return must execute, not only the interpreter"
    );
    let err_raw = err_payload_raw(vm.stack().last().expect("returned Result"));
    let local_raw = LAST_LOCAL_RAW.load(Ordering::SeqCst);
    assert_eq!(err_raw as u64, LAST_ERR_RAW.load(Ordering::SeqCst));
    assert_ne!(local_raw, 0);
    assert_local_closed_and_err_open_before_drop(&mut vm, local_raw, err_raw as u64);
    drop(vm);
    assert_eq!(
        CLOSURE_CLOSES.load(Ordering::SeqCst),
        2,
        "artifact VM drop must not close the already closed local or Err resource again"
    );
}

#[test]
fn result_try_postoperand_named_ok_carrier_transfers_resource_once() {
    let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_exit_observers();
    let source = r#"
        fn relay() -> Result<int, int> {
            let carrier = test::make_ok_result();
            let handle: resource<pr24.closure> = carrier?;
            test::close_closure(handle);
            Result::Ok(7)
        }
        let observed = match relay() { Result::Ok(n) => n, Result::Err(e) => e, };
        observed;
    "#;
    let mut vm = run_result_source(source);
    assert_eq!(vm.stack().last(), Some(&Value::Int(7)));
    assert_eq!(vm.execution_scope().resources().len(), 0);
    assert!(vm.execution_scope().is_active());
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);
    drop(vm);
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);
}

#[test]
fn result_try_postoperand_nested_call_carrier_keeps_only_returned_err() {
    let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_exit_observers();
    let source = r#"
        fn pass(value: Result<int, resource<pr24.closure>>) -> Result<int, resource<pr24.closure>> { value }
        fn relay() -> Result<int, resource<pr24.closure>> {
            let local: resource<pr24.closure> = test::make_local();
            let carrier = test::make_err_result();
            let value = pass(pass(carrier))?;
            test::close_closure(local);
            Result::Ok(value)
        }
        relay();
    "#;
    let mut vm = run_result_source(source);
    let err_raw = err_payload_raw(vm.stack().last().unwrap()) as u64;
    let local_raw = LAST_LOCAL_RAW.load(Ordering::SeqCst);
    assert_local_closed_and_err_open_before_drop(&mut vm, local_raw, err_raw);
    assert!(vm.execution_scope().is_active());
    drop(vm);
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 2);
}

#[test]
fn result_try_postoperand_named_err_in_partial_owned_argument_closes_temp() {
    let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_exit_observers();
    let source = r#"
        fn relay() -> Result<int, resource<pr24.closure>> {
            let carrier = test::make_err_result();
            let value = test::use_owned(test::make_local(), carrier?, test::later_arg());
            Result::Ok(value)
        }
        relay();
    "#;
    let mut vm = run_result_source(source);
    assert_eq!(USE_OWNED_CALLS.load(Ordering::SeqCst), 0);
    assert_eq!(LATER_ARG_CALLS.load(Ordering::SeqCst), 0);
    let err_raw = err_payload_raw(vm.stack().last().unwrap()) as u64;
    let local_raw = LAST_LOCAL_RAW.load(Ordering::SeqCst);
    assert_local_closed_and_err_open_before_drop(&mut vm, local_raw, err_raw);
    assert!(vm.execution_scope().is_active());
    drop(vm);
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 2);
}

#[test]
fn result_try_partial_owned_argument_closes_before_vm_drop() {
    let _guard = CLOSURE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_exit_observers();
    let source = r#"
        fn relay() -> Result<int, resource<pr24.closure>> {
            let value = test::use_owned(test::make_local(), test::make_err_result()?, test::later_arg());
            Result::Ok(value)
        }
        let carried = relay();
        carried;
    "#;
    let mut vm = run_result_source(source);
    assert_eq!(
        USE_OWNED_CALLS.load(Ordering::SeqCst),
        0,
        "callee must not run"
    );
    assert_eq!(
        LATER_ARG_CALLS.load(Ordering::SeqCst),
        0,
        "argument after ? must not run"
    );
    let err_raw = err_payload_raw(vm.stack().last().expect("returned Result"));
    let local_raw = LAST_LOCAL_RAW.load(Ordering::SeqCst);
    assert_local_closed_and_err_open_before_drop(&mut vm, local_raw, err_raw as u64);
    drop(vm);
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 2);
}

#[test]
fn result_try_partial_owned_array_closes_before_vm_drop() {
    let _guard = CLOSURE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_exit_observers();
    let source = r#"
        fn relay() -> Result<int, resource<pr24.closure>> {
            let value = test::use_owned_array([test::make_local()], test::make_err_result()?);
            Result::Ok(value)
        }
        let carried = relay();
        carried;
    "#;
    let mut vm = run_result_source(source);
    assert_eq!(USE_OWNED_CALLS.load(Ordering::SeqCst), 0);
    let err_raw = err_payload_raw(vm.stack().last().expect("returned Result"));
    let local_raw = LAST_LOCAL_RAW.load(Ordering::SeqCst);
    assert_local_closed_and_err_open_before_drop(&mut vm, local_raw, err_raw as u64);
    drop(vm);
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 2);
}

#[test]
fn result_try_ok_resource_continues_and_closes_once_before_vm_drop() {
    let _guard = CLOSURE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_exit_observers();
    let source = r#"
        fn relay() -> Result<int, int> {
            let handle: resource<pr24.closure> = test::make_ok_result()?;
            test::close_closure(handle);
            Result::Ok(1)
        }
        let flag = match relay() {
            Result::Ok(v) => v,
            Result::Err(e) => e,
        };
        flag;
    "#;
    let mut vm = run_result_source(source);
    assert_eq!(vm.stack().last().cloned(), Some(Value::Int(1)));
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);
    assert_eq!(vm.execution_scope().resources().len(), 0);
    let local_raw = LAST_LOCAL_RAW.load(Ordering::SeqCst);
    assert_ne!(local_raw, 0);
    drop(vm);
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);
}

#[test]
fn result_try_rejects_reuse_of_moved_ok_resource() {
    let source = r#"
        fn relay() -> Result<int, int> {
            let handle: resource<pr24.closure> = test::make_ok_result()?;
            test::close_closure(handle);
            test::close_closure(handle);
            Result::Ok(1)
        }
        relay();
    "#;
    let options = CompileSourceFileOptions::new().with_host_api_catalog(result_resource_catalog());
    let error =
        match compile_source_with_flavor_and_options(source, vm::SourceFlavor::RustScript, options)
        {
            Err(error) => error,
            Ok(_) => panic!("moved Ok resource must not be reused"),
        };
    let text = error.to_string();
    assert!(
        text.contains("moved") || text.contains("use"),
        "reuse after move should be rejected, got {error:?}"
    );
}

#[test]
fn result_try_does_not_close_borrowed_caller_resource() {
    let _guard = CLOSURE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_exit_observers();
    let source = r#"
        fn relay(borrowed: resource<pr24.closure>) -> Result<int, int> {
            let probe = test::touch_borrow(borrowed);
            let value: int = test::make_err_int()?;
            Result::Ok(probe + value)
        }
        let owned: resource<pr24.closure> = test::make_local();
        let carried = relay(owned);
        test::close_closure(owned);
        carried;
    "#;
    let mut vm = run_result_source(source);
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);
    assert_eq!(vm.execution_scope().resources().len(), 0);
    drop(vm);
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);
}

#[test]
fn result_try_keeps_borrowed_param_usable_when_ok_path_uses_it_after_try() {
    let _guard = CLOSURE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_exit_observers();
    let source = r#"
        fn relay(borrowed: resource<pr24.closure>) -> Result<int, int> {
            let value: int = test::make_err_int()?;
            let probe = test::touch_borrow(borrowed);
            Result::Ok(probe + value)
        }
        let owned: resource<pr24.closure> = test::make_local();
        let carried = relay(owned);
        let still = test::touch_borrow(owned);
        test::close_closure(owned);
        still;
        carried;
    "#;
    let mut vm = run_result_source(source);
    assert_eq!(err_payload_raw(vm.stack().last().expect("Err result")), 9);
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);
    assert_eq!(vm.execution_scope().resources().len(), 0);
    drop(vm);
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);
}

#[test]
fn result_try_does_not_close_persistent_capture_used_after_try() {
    let _guard = CLOSURE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_exit_observers();
    let source = r#"
        let owned: resource<pr24.closure> = test::make_local();
        fn relay() -> Result<int, int> {
            let value: int = test::make_err_int()?;
            let probe = test::touch_borrow(owned);
            if false => {
                test::close_closure(owned);
                0
            } else => {
                0
            };
            Result::Ok(probe + value)
        }
        let first = relay();
        let second = relay();
        first;
    "#;
    let mut vm = run_result_source(source);
    let local_raw = LAST_LOCAL_RAW.load(Ordering::SeqCst);
    assert_ne!(local_raw, 0);
    assert!(
        closure_is_open(&mut vm, local_raw),
        "persistent capture must stay open across Err returns"
    );
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 0);
    assert_eq!(err_payload_raw(vm.stack().last().expect("first Err")), 9);
    drop(vm);
}

#[test]
fn result_try_match_owned_payload_is_closed_on_later_err() {
    let _guard = CLOSURE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_exit_observers();
    let source = r#"
        fn relay() -> Result<int, int> {
            let value = test::make_ok_result();
            match value {
                Result::Ok(handle) => if true => {
                    let n: int = test::make_err_int()?;
                    Result::Ok(n)
                } else => {
                    Result::Ok(0)
                },
                Result::Err(code) => Result::Err(code),
            }
        }
        let carried = relay();
        carried;
    "#;
    let mut vm = run_result_source(source);
    let local_raw = LAST_LOCAL_RAW.load(Ordering::SeqCst);
    assert_ne!(local_raw, 0);
    assert!(
        !closure_is_open(&mut vm, local_raw),
        "match payload must close before VM drop"
    );
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);
    drop(vm);
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);
}

#[test]
fn result_try_closes_consumed_param_still_held_on_err() {
    let _guard = CLOSURE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_exit_observers();
    let source = r#"
        fn relay(owned: resource<pr24.closure>) -> Result<int, int> {
            let value: int = test::make_err_int()?;
            test::close_closure(owned);
            Result::Ok(value)
        }
        let owned: resource<pr24.closure> = test::make_local();
        let carried = relay(owned);
        carried;
    "#;
    let mut vm = run_result_source(source);
    let local_raw = LAST_LOCAL_RAW.load(Ordering::SeqCst);
    assert_ne!(local_raw, 0);
    assert!(
        !closure_is_open(&mut vm, local_raw),
        "consumed param must close on Err"
    );
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);
    drop(vm);
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);
}

#[test]
fn result_try_closes_owned_local_after_borrow_with_no_later_ordinary_use() {
    let _guard = CLOSURE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_exit_observers();
    let source = r#"
        fn relay() -> Result<int, int> {
            let first: resource<pr24.closure> = test::make_local();
            let probe = test::touch_borrow(first);
            let scalar_a = probe + 1;
            let scalar_b = scalar_a + 2;
            let scalar_c = scalar_b + 3;
            let second: resource<pr24.closure> = test::make_local();
            let value: int = test::make_err_int()?;
            Result::Ok(scalar_c + value)
        }
        let carried = relay();
        carried;
    "#;
    let mut vm = run_result_source(source);
    let local_raw = LAST_LOCAL_RAW.load(Ordering::SeqCst);
    assert_ne!(local_raw, 0);
    assert!(
        !closure_is_open(&mut vm, local_raw),
        "cleanup-only local must close on Err before VM drop"
    );
    assert_eq!(
        CLOSURE_CLOSES.load(Ordering::SeqCst),
        2,
        "both owned resources must close on Err, not only the last allocated slot"
    );
    drop(vm);
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 2);
}

#[test]
fn result_try_snapshots_bare_read_before_later_ok_mutation() {
    let source = r#"
        fn relay() -> Result<int, int> {
            let mut x = 1;
            fn bump() -> Result<int, int> {
                x = 11;
                Result::Ok(0)
            }
            fn keep(first: int, second: int) -> Result<int, int> {
                Result::Ok(first)
            }
            keep(x, bump()?)
        }
        let carried = relay();
        carried;
    "#;
    let vm = run_result_source(source);
    let value = vm.stack().last().expect("returned Result");
    let Value::Map(map) = value else {
        panic!("expected Result map, got {value:?}");
    };
    assert_eq!(map.get(&Value::string("tag")), Some(&Value::Bool(true)));
    assert_eq!(
        map.get(&Value::string("payload")),
        Some(&Value::Int(1)),
        "bare read of x must be snapshotted before bump()? mutates it, got {value:?}"
    );
}

#[test]
fn local_call_snapshots_callee_and_closes_resource_before_later_try() {
    let _guard = CLOSURE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_exit_observers();
    let source = r#"
        fn relay() -> Result<int, resource<pr24.closure>> {
            let callee = |resource, later| if true => {
                test::use_owned(resource, 0, later)
            } else => { 0 };
            let value = callee(test::make_local(), test::make_err_result()?);
            Result::Ok(value)
        }
        let carried = relay();
        carried;
    "#;
    let mut vm = run_result_source(source);
    assert_eq!(USE_OWNED_CALLS.load(Ordering::SeqCst), 0);
    assert_eq!(
        err_payload_raw(vm.stack().last().expect("Result")),
        LAST_ERR_RAW.load(Ordering::SeqCst) as i64
    );
    let local_raw = LAST_LOCAL_RAW.load(Ordering::SeqCst);
    assert!(!closure_is_open(&mut vm, local_raw));
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);
}

#[test]
fn local_call_uses_snapshotted_callee_after_reassignment() {
    let _guard = CLOSURE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_exit_observers();
    let source = r#"
        fn relay() -> Result<int, int> {
            let mut callee = |later| if true => { 1 } else => { 0 };
            let value = callee(if true => {
                callee = |later| if true => { 100 } else => { 0 };
                0
            } else => { 0 });
            Result::Ok(value)
        }
        let carried = relay();
        carried;
    "#;
    let vm = run_result_source(source);
    let value = vm.stack().last().expect("Result");
    let Value::Map(map) = value else {
        panic!("expected Result map, got {value:?}");
    };
    assert_eq!(
        map.get(&Value::string("payload")),
        Some(&Value::Int(1)),
        "callee must be snapshotted before the later try reassigns it, got {value:?}"
    );
}

#[test]
fn borrow_and_borrow_mut_stay_passing_across_later_try() {
    let _guard = CLOSURE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_exit_observers();
    let source = r#"
        fn relay() -> Result<int, int> {
            let mut owned: resource<pr24.closure> = test::make_local();
            let borrowed = test::touch_borrow(owned);
            let mutated = test::touch_borrow_mut(&mut owned, test::make_err_int()?);
            Result::Ok(borrowed + mutated)
        }
        let carried = relay();
        carried;
    "#;
    let mut vm = run_result_source(source);
    assert_eq!(BORROW_BOUNDARY_CALLS.load(Ordering::SeqCst), 0);
    let local_raw = LAST_LOCAL_RAW.load(Ordering::SeqCst);
    assert!(!closure_is_open(&mut vm, local_raw));
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);
    drop(vm);
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);
}

#[test]
fn result_try_conditional_take_join_closes_only_present_owners_for_both_flags() {
    let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for (flag, backend) in [false, true]
        .into_iter()
        .flat_map(|flag| (0..3).map(move |backend| (flag, backend)))
    {
        reset_exit_observers();
        let source = format!(
            r#"
            fn relay(flag: bool) -> Result<int, resource<pr24.closure>> {{
                let owned: resource<pr24.closure> = test::make_local();
                let sibling: resource<pr24.closure> = test::make_local();
                if flag {{ let closed = test::close_closure(owned); }}
                let n = test::make_err_result()?;
                test::close_closure(sibling);
                Result::Ok(n)
            }}
            relay({flag});
        "#
        );
        let mut machine = run_result_source_backend(&source, backend);
        let err = err_payload_raw(machine.stack().last().unwrap()) as u64;
        assert_eq!(
            CLOSURE_CLOSES.load(Ordering::SeqCst),
            2,
            "both logical owners close exactly once: flag={flag}"
        );
        assert_eq!(machine.execution_scope().resources().len(), 1);
        assert!(closure_is_open(&mut machine, err));
        machine
            .execution_scope()
            .close_resource::<ClosureResource>(
                ResourceHandle::from_raw(err).unwrap(),
                ResourceCloseReason::Requested,
            )
            .unwrap();
        assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 3);
        assert_eq!(machine.execution_scope().resources().len(), 0);
        drop(machine);
        assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 3);
    }
}

#[test]
fn result_try_conditional_take_reassignment_restores_present_owner() {
    let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for (flag, backend) in [false, true]
        .into_iter()
        .flat_map(|flag| (0..3).map(move |backend| (flag, backend)))
    {
        reset_exit_observers();
        let source = format!(
            r#"
            fn relay(flag: bool) -> Result<int, int> {{
                let mut owned: resource<pr24.closure> = test::make_local();
                if flag {{ let closed = test::close_closure(owned); owned = test::make_local(); }}
                let n = test::make_err_int()?;
                test::close_closure(owned);
                Result::Ok(n)
            }}
            relay({flag});
        "#
        );
        let mut machine = run_result_source_backend(&source, backend);
        assert_eq!(err_payload_raw(machine.stack().last().unwrap()), 9);
        assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1 + usize::from(flag));
        assert_eq!(machine.execution_scope().resources().len(), 0);
        drop(machine);
        assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1 + usize::from(flag));
    }
}

#[test]
fn result_try_named_match_payload_body_is_owned_in_both_result_arms() {
    let _guard = CLOSURE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for (ok, backend) in [false, true]
        .into_iter()
        .flat_map(|ok| (0..3).map(move |backend| (ok, backend)))
    {
        reset_exit_observers();
        let (ty, carrier, arm) = if ok {
            (
                "Result<Holder, int>",
                "Result::Ok(holder)",
                "Result::Ok(payload)",
            )
        } else {
            (
                "Result<int, Holder>",
                "Result::Err(holder)",
                "Result::Err(payload)",
            )
        };
        let other = if ok {
            "Result::Err(_)"
        } else {
            "Result::Ok(_)"
        };
        let source = format!(
            r#"
            struct Holder {{ value: resource<pr24.closure> }}
            fn relay() -> Result<int, int> {{
                let holder: Holder = {{ value: test::make_local() }};
                let carried: {ty} = {carrier};
                match carried {{
                    {arm} => if true => {{
                        let n = test::make_err_int()?;
                        Result::Ok(n)
                    }} else => {{ Result::Ok(0) }},
                    {other} => Result::Ok(0),
                }}
            }}
            relay();
        "#
        );
        let mut machine = run_result_source_backend(&source, backend);
        assert_eq!(err_payload_raw(machine.stack().last().unwrap()), 9);
        assert_eq!(
            CLOSURE_CLOSES.load(Ordering::SeqCst),
            1,
            "non-generic Named payload closes before drop: ok={ok}"
        );
        assert_eq!(machine.execution_scope().resources().len(), 0);
        drop(machine);
        assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn distinct_resource_keys_scalar_and_closure_close_only_owned_resources() {
    let _guard = CLOSURE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_exit_observers();
    let source = r#"
        fn relay() -> Result<int, int> {
            let owned: resource<pr24.closure> = test::make_local();
            let block: resource<pr24.block> = test::make_block();
            let probe = test::touch_borrow(owned);
            let closer = || if true => { 1 } else => { 0 };
            let scalar = probe + closer();
            let value = test::make_err_int()?;
            Result::Ok(scalar + value)
        }
        let carried = relay();
        carried;
    "#;
    let mut vm = run_result_source(source);
    let local_raw = LAST_LOCAL_RAW.load(Ordering::SeqCst);
    assert!(!closure_is_open(&mut vm, local_raw));
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);
    assert_eq!(BLOCK_CLOSES.load(Ordering::SeqCst), 1);
    drop(vm);
    assert_eq!(CLOSURE_CLOSES.load(Ordering::SeqCst), 1);
    assert_eq!(BLOCK_CLOSES.load(Ordering::SeqCst), 1);
}
