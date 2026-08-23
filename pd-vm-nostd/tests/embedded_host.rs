use pd_vm_nostd::{
    HostBinding, HostError, HostImportBindingError, Value as EmbeddedValue, Vm as EmbeddedVm,
    VmError, VmStatus, decode_program,
};
use vm::{
    CompileSourceFileOptions, HostApiBuilder, HostFunctionSchema, HostParamPassing,
    HostParamSchema, HostTypeSchema, ResourceTypeKey, ResourceTypeSchema, SourceFlavor,
    compile_source_for_repl, compile_source_with_flavor_and_options, encode_program,
};

#[derive(Default)]
struct BoardState {
    pin: i64,
    high: bool,
}

fn compile_embedded(source: &str) -> pd_vm_nostd::Program {
    let compiled = compile_source_for_repl(source).expect("RustScript source should compile");
    let bytes = encode_program(&compiled.program.with_local_count(compiled.locals))
        .expect("compiled program should encode");
    decode_program(&bytes).expect("embedded runtime should decode compiler VMBC")
}

fn gpio_set(
    state: &mut BoardState,
    args: &[EmbeddedValue],
) -> Result<Option<EmbeddedValue>, HostError> {
    let [EmbeddedValue::Int(pin), EmbeddedValue::Bool(high)] = args else {
        return Err(HostError::new("gpio_set expects int and bool"));
    };
    state.pin = *pin;
    state.high = *high;
    Ok(None)
}

fn dispatch_host(
    state: &mut BoardState,
    name: &str,
    args: &[EmbeddedValue],
) -> Result<Option<EmbeddedValue>, HostError> {
    if name != "gpio_set" {
        return Err(HostError::new("unexpected host import"));
    }
    gpio_set(state, args)
}

#[test]
fn script_call_frames_execute_in_embedded_runtime() {
    let program = compile_embedded(
        r#"
            fn inc(x) { x + 1 }
            fn twice(x) { inc(inc(x)) }
            twice(40);
        "#,
    );
    let mut vm = EmbeddedVm::new(program);

    assert_eq!(vm.run(), Ok(VmStatus::Halted));
    assert_eq!(vm.stack(), &[EmbeddedValue::Int(42)]);
}

#[test]
fn script_call_depth_limit_is_configurable() {
    let program = compile_embedded(
        r#"
            fn recurse(value: int) -> int { recurse(value) }
            recurse(1);
        "#,
    );
    let mut vm = EmbeddedVm::new(program);

    assert_eq!(vm.max_script_call_depth(), 1024);
    assert!(matches!(
        vm.set_max_script_call_depth(0),
        Err(VmError::InvalidCallStackLimit(0))
    ));
    vm.set_max_script_call_depth(3)
        .expect("positive call depth should be accepted");
    assert_eq!(vm.max_script_call_depth(), 3);
    assert_eq!(vm.run(), Err(VmError::CallStackOverflow));
}

#[test]
fn static_host_binding_mutates_board_context() {
    let program = compile_embedded(
        r#"
            fn gpio_set(pin: int, high: bool);
            gpio_set(25, true);
        "#,
    );
    let bindings = [HostBinding::new("gpio_set", 2, gpio_set)];
    let mut vm = EmbeddedVm::with_host_bindings(program, BoardState::default(), &bindings)
        .expect("host imports should bind");

    assert_eq!(vm.run(), Ok(VmStatus::Halted));
    assert_eq!(vm.context().pin, 25);
    assert!(vm.context().high);
}

#[test]
fn dispatcher_receives_import_name_and_arguments() {
    let program = compile_embedded(
        r#"
            fn gpio_set(pin: int, high: bool);
            gpio_set(25, true);
        "#,
    );
    let mut vm = EmbeddedVm::with_host_dispatcher(program, BoardState::default(), dispatch_host);

    assert_eq!(vm.run(), Ok(VmStatus::Halted));
    assert_eq!(vm.context().pin, 25);
    assert!(vm.context().high);
}

#[test]
fn missing_import_and_arity_mismatch_fail_during_binding() {
    let program = compile_embedded(
        r#"
            fn gpio_set(pin: int, high: bool);
            gpio_set(25, true);
        "#,
    );
    assert!(matches!(
        EmbeddedVm::with_host_bindings(program.clone(), BoardState::default(), &[]),
        Err(VmError::UnboundImport(name)) if name == "gpio_set"
    ));

    let wrong_arity = [HostBinding::new("gpio_set", 1, gpio_set)];
    assert!(matches!(
        EmbeddedVm::with_host_bindings(program, BoardState::default(), &wrong_arity),
        Err(VmError::InvalidCallArity {
            expected: 1,
            got: 2,
            ..
        })
    ));
}

#[test]
fn fuel_can_pause_and_resume_a_finite_loop() {
    let program = compile_embedded(
        r#"
            let mut count = 0;
            while count < 4 {
                count = count + 1;
            }
            count;
        "#,
    );
    let mut vm = EmbeddedVm::new(program);
    vm.set_fuel(4);

    assert_eq!(
        vm.run(),
        Err(VmError::OutOfFuel {
            needed: 1,
            remaining: 0,
        })
    );
    assert_eq!(vm.fuel(), Some(0));

    vm.add_fuel(100).expect("fuel addition should succeed");
    assert_eq!(vm.run(), Ok(VmStatus::Halted));
    assert_eq!(vm.stack().last(), Some(&EmbeddedValue::Int(4)));
}

/// The shared catalog used by the full-stack exact-binding tests. Every
/// function's schema and the catalog fingerprint are produced by the real
/// compiler through `compile_source_with_flavor_and_options`, so the decoded
/// no_std import schema genuinely corresponds to the catalog.
fn exact_catalog() -> std::sync::Arc<vm::HostApiCatalog> {
    let file = ResourceTypeKey::new("io.file").expect("valid io.file key");
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(file.clone(), "file"));
    builder.function(HostFunctionSchema::with_return(
        "acme::add",
        vec![HostParamSchema::value("value", HostTypeSchema::Int)],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "acme::add",
        vec![HostParamSchema::value("value", HostTypeSchema::Float)],
        HostTypeSchema::Float,
    ));
    builder.function(HostFunctionSchema::with_return(
        "acme::greet",
        vec![HostParamSchema::value("name", HostTypeSchema::String)],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "acme::read",
        vec![HostParamSchema::with_passing(
            "file",
            HostTypeSchema::Resource(file.clone()),
            HostParamPassing::Borrow,
        )],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "acme::open",
        vec![HostParamSchema::value("path", HostTypeSchema::String)],
        HostTypeSchema::Resource(file.clone()),
    ));
    builder.function(HostFunctionSchema::with_return(
        "acme::store",
        vec![HostParamSchema::with_passing(
            "file",
            HostTypeSchema::Resource(file),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::Int,
    ));
    std::sync::Arc::new(builder.build().expect("test catalog must build"))
}

/// Compiles RustScript source against the real catalog, encodes the V13 VMBC
/// with the std encoder, and decodes it in the no_std runtime. This is the
/// genuine compiler → V13 → no_std pipeline used by every exact test below.
fn compile_catalog_program(source: &str) -> pd_vm_nostd::Program {
    let compiled = compile_source_with_flavor_and_options(
        source,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(exact_catalog()),
    )
    .expect("catalog source should compile");
    let bytes = encode_program(&compiled.program.with_local_count(compiled.locals))
        .expect("std VMBC encoder should encode the compiled program");
    decode_program(&bytes).expect("no_std runtime should decode compiler VMBC")
}

fn exact_add_host(
    _state: &mut BoardState,
    args: &[EmbeddedValue],
) -> Result<Option<EmbeddedValue>, HostError> {
    let [EmbeddedValue::Int(value)] = args else {
        return Err(HostError::new("acme::add expects one int"));
    };
    Ok(Some(EmbeddedValue::Int(*value + 2)))
}

fn exact_add_float_host(
    _state: &mut BoardState,
    args: &[EmbeddedValue],
) -> Result<Option<EmbeddedValue>, HostError> {
    let [EmbeddedValue::Float(value)] = args else {
        return Err(HostError::new("acme::add expects one float"));
    };
    Ok(Some(EmbeddedValue::Float(*value + 0.5)))
}

fn exact_greet_host(
    _state: &mut BoardState,
    args: &[EmbeddedValue],
) -> Result<Option<EmbeddedValue>, HostError> {
    let [EmbeddedValue::String(_)] = args else {
        return Err(HostError::new("acme::greet expects one string"));
    };
    Ok(Some(EmbeddedValue::Int(7)))
}

fn exact_store_host(
    _state: &mut BoardState,
    args: &[EmbeddedValue],
) -> Result<Option<EmbeddedValue>, HostError> {
    let [_file] = args else {
        return Err(HostError::new("acme::store expects one file"));
    };
    Ok(Some(EmbeddedValue::Int(9)))
}

fn decoded_import_schema(
    program: &pd_vm_nostd::Program,
    name: &str,
) -> pd_vm_nostd::HostImportSchema {
    program
        .imports()
        .iter()
        .find(|import| import.name == name)
        .expect("compiled program must import the declared host function")
        .schema
        .clone()
        .expect("catalog-resolved import must carry an exact schema")
}

/// The no_std decoded schema must carry the same catalog fingerprint the std
/// compiler produced, proving the fingerprint is not an embedded-fixture
/// fabrication.
#[test]
fn decoded_import_schema_carries_catalog_fingerprint() {
    let program = compile_catalog_program("use acme;\nacme::add(40);\n");
    let import = program
        .imports()
        .iter()
        .find(|import| import.name == "acme::add")
        .expect("compiled add import");
    let schema = import.schema.as_ref().expect("exact schema");
    assert_eq!(
        schema.fingerprint.as_u64(),
        exact_catalog().fingerprint().as_u64(),
        "decoded fingerprint must equal the catalog fingerprint"
    );
    assert_eq!(schema.params.len(), 1);
    assert_eq!(schema.params[0].schema, pd_vm_nostd::TypeSchema::Int);
    assert_eq!(
        schema.params[0].passing,
        pd_vm_nostd::HostParamPassing::Value
    );
    assert_eq!(schema.return_type, pd_vm_nostd::TypeSchema::Int);
    // The compiler derives import arity from the schema parameter count; the
    // no_std decoder preserves that coupling across the wire.
    assert_eq!(import.arity, 1);
    assert_eq!(usize::from(import.arity), schema.params.len());
}

#[test]
fn exact_binding_runs_embedded_host() {
    let program = compile_catalog_program("use acme;\nacme::add(40);\n");
    let schema = decoded_import_schema(&program, "acme::add");

    let bindings = [HostBinding::exact("acme::add", 1, schema, exact_add_host)
        .expect("constructor validates arity")];
    let mut vm = EmbeddedVm::with_host_bindings(program, BoardState::default(), &bindings)
        .expect("matching exact schema and fingerprint should bind");

    assert_eq!(vm.run(), Ok(VmStatus::Halted));
    assert_eq!(vm.stack(), &[EmbeddedValue::Int(42)]);
}

#[test]
fn exact_binding_rejects_fingerprint_mismatch() {
    let program = compile_catalog_program("use acme;\nacme::add(40);\n");
    let mut wrong_schema = decoded_import_schema(&program, "acme::add");
    // Corrupt the fingerprint only; everything else stays identical.
    wrong_schema.fingerprint =
        pd_vm_nostd::HostApiFingerprint::from_wire(wrong_schema.fingerprint.as_u64() ^ 0xDEAD);
    let bindings = [
        HostBinding::exact("acme::add", 1, wrong_schema, exact_add_host)
            .expect("constructor validates arity"),
    ];

    assert!(matches!(
        EmbeddedVm::with_host_bindings(program, BoardState::default(), &bindings),
        Err(VmError::HostImportBinding(HostImportBindingError::MissingExact {
            import
        })) if import == "acme::add"
    ));
}

#[test]
fn exact_binding_rejects_param_type_mismatch() {
    // The compiler resolves `acme::greet` against the catalog: its decoded
    // import schema has a `string` parameter. A binding whose parameter schema
    // is `int` cannot satisfy the exact import.
    let program = compile_catalog_program("use acme;\nacme::greet(\"hi\");\n");
    let mut wrong_schema = decoded_import_schema(&program, "acme::greet");
    wrong_schema.params[0].schema = pd_vm_nostd::TypeSchema::Int;
    let bindings = [
        HostBinding::exact("acme::greet", 1, wrong_schema, exact_greet_host)
            .expect("constructor validates arity"),
    ];

    assert!(matches!(
        EmbeddedVm::with_host_bindings(program, BoardState::default(), &bindings),
        Err(VmError::HostImportBinding(HostImportBindingError::MissingExact {
            import
        })) if import == "acme::greet"
    ));
}

#[test]
fn exact_binding_rejects_passing_mode_mismatch() {
    // The compiler resolves `acme::read` (Borrow `io.file` passing) against the
    // catalog. A binding that keeps every field but switches the passing mode
    // to Value is a different exact key and must be rejected.
    let program =
        compile_catalog_program("use acme;\nlet f = acme::open(\"x\");\nacme::read(&f);\n");
    let open_schema = decoded_import_schema(&program, "acme::open");
    let mut wrong_schema = decoded_import_schema(&program, "acme::read");
    wrong_schema.params[0].passing = pd_vm_nostd::HostParamPassing::Value;
    let bindings = [
        HostBinding::exact("acme::open", 1, open_schema, exact_greet_host)
            .expect("constructor validates arity"),
        HostBinding::exact("acme::read", 1, wrong_schema, exact_greet_host)
            .expect("constructor validates arity"),
    ];

    assert!(matches!(
        EmbeddedVm::with_host_bindings(program, BoardState::default(), &bindings),
        Err(VmError::HostImportBinding(HostImportBindingError::MissingExact {
            import
        })) if import == "acme::read"
    ));
}

#[test]
fn exact_binding_rejects_resource_key_mismatch() {
    // The compiler resolves `acme::store` (TakeOwned `io.file`) against the
    // catalog. A binding that keeps the passing mode but uses a different
    // resource key is a different exact key and must be rejected.
    let program = compile_catalog_program("use acme;\nacme::store(acme::open(\"x\"));\n");
    let open_schema = decoded_import_schema(&program, "acme::open");
    let mut wrong_store_schema = decoded_import_schema(&program, "acme::store");
    wrong_store_schema.params[0].schema = pd_vm_nostd::TypeSchema::Resource(
        pd_vm_nostd::ResourceTypeKey::from_wire("io.other".to_string()).expect("valid wire key"),
    );
    let bindings = [
        HostBinding::exact("acme::open", 1, open_schema, exact_greet_host)
            .expect("constructor validates arity"),
        HostBinding::exact("acme::store", 1, wrong_store_schema, exact_store_host)
            .expect("constructor validates arity"),
    ];

    assert!(matches!(
        EmbeddedVm::with_host_bindings(program, BoardState::default(), &bindings),
        Err(VmError::HostImportBinding(HostImportBindingError::MissingExact {
            import
        })) if import == "acme::store"
    ));
}

#[test]
fn exact_binding_rejects_return_type_mismatch() {
    // A binding whose return schema differs from the compiled import's exact
    // return schema (here `int` vs the `float` overload) cannot satisfy it.
    let program = compile_catalog_program("use acme;\nacme::add(1.5);\n");
    let mut wrong_schema = decoded_import_schema(&program, "acme::add");
    wrong_schema.return_type = pd_vm_nostd::TypeSchema::Int;
    let bindings = [
        HostBinding::exact("acme::add", 1, wrong_schema, exact_add_float_host)
            .expect("constructor validates arity"),
    ];

    assert!(matches!(
        EmbeddedVm::with_host_bindings(program, BoardState::default(), &bindings),
        Err(VmError::HostImportBinding(HostImportBindingError::MissingExact {
            import
        })) if import == "acme::add"
    ));
}

#[test]
fn exact_binding_rejects_name_mismatch() {
    let program = compile_catalog_program("use acme;\nacme::add(40);\n");
    let schema = decoded_import_schema(&program, "acme::add");
    let bindings = [HostBinding::exact("acme::other", 1, schema, exact_add_host)
        .expect("constructor validates arity")];

    assert!(matches!(
        EmbeddedVm::with_host_bindings(program, BoardState::default(), &bindings),
        Err(VmError::HostImportBinding(HostImportBindingError::MissingExact {
            import
        })) if import == "acme::add"
    ));
}

#[test]
fn compiled_import_preserves_coarse_return_type_invariant() {
    // This is a positive compiler-to-decoder invariant check. The malformed
    // V13 decoder rejection is covered by the focused mutation test in
    // `embedded_vmbc.rs`.
    let program = compile_catalog_program("use acme;\nacme::add(1.5);\n");
    let import = program
        .imports()
        .iter()
        .find(|import| import.name == "acme::add")
        .expect("compiled add import");
    let schema = import.schema.as_ref().expect("exact schema");
    assert_eq!(import.return_type, pd_vm_nostd::ValueType::Float);
    assert_eq!(schema.return_type, pd_vm_nostd::TypeSchema::Float);
}

#[test]
fn compiled_import_preserves_schema_arity_invariant() {
    // This is a positive compiler-to-decoder invariant check. The malformed
    // V13 decoder rejection is covered by the focused mutation test in
    // `embedded_vmbc.rs`.
    let program = compile_catalog_program("use acme;\nacme::add(40);\n");
    let schema = decoded_import_schema(&program, "acme::add");
    assert_eq!(usize::from(program.imports()[0].arity), schema.params.len());
}

#[test]
fn exact_binding_never_binds_through_name_only_fallback() {
    let program = compile_catalog_program("use acme;\nacme::add(40);\n");
    let bindings = [HostBinding::new("acme::add", 1, exact_add_host)];

    // A legacy name+arity binding must not satisfy an exact-schema import.
    assert!(matches!(
        EmbeddedVm::with_host_bindings(program, BoardState::default(), &bindings),
        Err(VmError::HostImportBinding(HostImportBindingError::MissingExact {
            import
        })) if import == "acme::add"
    ));
}

#[test]
fn exact_overload_disambiguates_through_full_stack() {
    // The catalog declares two `acme::add` overloads (int and float). The
    // compiler resolves the float call to the float overload; the int binding
    // and a name-only binding are both present and must not be picked.
    let program = compile_catalog_program("use acme;\nacme::add(1.5);\n");
    let schema = decoded_import_schema(&program, "acme::add");
    let int_program = compile_catalog_program("use acme;\nacme::add(40);\n");
    let int_schema = decoded_import_schema(&int_program, "acme::add");
    assert_eq!(schema.return_type, pd_vm_nostd::TypeSchema::Float);
    assert_eq!(schema.params[0].schema, pd_vm_nostd::TypeSchema::Float);
    assert_eq!(int_schema.return_type, pd_vm_nostd::TypeSchema::Int);
    assert_eq!(int_schema.params[0].schema, pd_vm_nostd::TypeSchema::Int);

    let bindings = [
        HostBinding::new("acme::add", 1, exact_add_host),
        HostBinding::exact("acme::add", 1, int_schema, exact_add_host)
            .expect("constructor validates arity"),
        HostBinding::exact("acme::add", 1, schema, exact_add_float_host)
            .expect("constructor validates arity"),
    ];
    let mut vm = EmbeddedVm::with_host_bindings(program, BoardState::default(), &bindings)
        .expect("float overload should bind through the full stack");

    assert_eq!(vm.run(), Ok(VmStatus::Halted));
    assert_eq!(vm.stack(), &[EmbeddedValue::Float(2.0)]);
}

#[test]
fn duplicate_exact_bindings_rejected_through_full_stack() {
    // The same decoded schema registered twice produces an identical exact key;
    // the resolver must reject the ambiguity instead of first-matching.
    let program = compile_catalog_program("use acme;\nacme::add(40);\n");
    let schema = decoded_import_schema(&program, "acme::add");
    let bindings = [
        HostBinding::exact("acme::add", 1, schema.clone(), exact_add_host)
            .expect("constructor validates arity"),
        HostBinding::exact("acme::add", 1, schema, exact_add_host)
            .expect("constructor validates arity"),
    ];

    assert!(matches!(
        EmbeddedVm::with_host_bindings(program, BoardState::default(), &bindings),
        Err(VmError::HostImportBinding(HostImportBindingError::Duplicate { import }))
            if import == "acme::add"
    ));
}
