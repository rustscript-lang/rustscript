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
    let _guard = CLOSURE_TEST_LOCK.lock().expect("closure test lock");
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
    let _guard = CLOSURE_TEST_LOCK.lock().expect("closure test lock");
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
