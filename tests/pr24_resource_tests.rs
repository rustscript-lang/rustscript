#![cfg(feature = "runtime")]

use std::sync::atomic::{AtomicUsize, Ordering};
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
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(
            token.handle().raw() as i64,
        ))))
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
        "test::close_closure",
        vec![HostParamSchema::with_passing(
            "resource",
            HostTypeSchema::Resource(resource_key),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::Null,
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
        Ok(CallOutcome::Return(CallReturn::one(result_map(
            true,
            Value::Int(token.handle().raw() as i64),
            false,
        ))))
    }
}

struct MakeErrResult;
impl HostFunction for MakeErrResult {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        let token = vm
            .host_context()
            .push_resource(ClosureResource)
            .map_err(|error| VmError::HostError(error.to_string()))?;
        Ok(CallOutcome::Return(CallReturn::one(result_map(
            false,
            Value::Int(token.handle().raw() as i64),
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
    vm.bind_function("test::close_closure", Box::new(CloseClosureResource));
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
