//! Milestone 7: `CallScript` parity in the no_std + alloc runtime.
//!
//! Programs are produced by the std VMBC encoder (V13) or hand-built with
//! `CallScript` bytecode (0x1A, prototype_id:u32 LE, argc:u8) so the wire
//! contract and the typed validation/execution failures are pinned
//! independently of the compiler.

use pd_vm_nostd::{
    Value as EmbeddedValue, Vm as EmbeddedVm, VmError, VmStatus as EmbeddedVmStatus, WireError,
    decode_program,
};
use vm::{
    CallableKind, CallablePrototype, CallableTarget, FunctionRegion, OpCode, Program,
    ScriptFunction, compile_source, encode_program,
};

/// Build a main-crate program whose root code is `code` with one script
/// function (entry at `code.len()`) described by `prototype`.
fn raw_call_script_program(code: Vec<u8>, prototype: CallablePrototype) -> Program {
    let function_entry = code.len() as u32;
    let function_end = function_entry + 1;
    let mut code = code;
    code.push(OpCode::Ret as u8);
    Program::new(Vec::new(), code)
        .with_local_count(1)
        .with_callable_metadata(
            vec![ScriptFunction {
                entry_ip: function_entry,
                end_ip: function_end,
            }],
            vec![prototype],
            vec![
                FunctionRegion {
                    start_ip: 0,
                    end_ip: function_entry,
                    prototype_id: None,
                },
                FunctionRegion {
                    start_ip: function_entry,
                    end_ip: function_end,
                    prototype_id: Some(0),
                },
            ],
            vec![],
        )
}

fn function_item_prototype(
    target: CallableTarget,
    arity: u8,
    capture_slots: Vec<u16>,
    self_slot: Option<u16>,
) -> CallablePrototype {
    CallablePrototype {
        kind: CallableKind::FunctionItem,
        target,
        arity,
        frame_local_count: 1,
        parameter_slots: (0..arity).map(u16::from).collect(),
        capture_source_slots: Vec::new(),
        capture_slots,
        capture_modes: Vec::new(),
        self_slot,
        schema: None,
    }
}

#[test]
fn call_script_executes_direct_call() {
    let compiled = compile_source("fn add2(value: int) -> int { value + 2 } add2(40);")
        .expect("direct call source should compile");
    let bytes = encode_program(&compiled.program.with_local_count(compiled.locals))
        .expect("direct call program should encode as VMBC v13");
    let program = decode_program(&bytes).expect("no-std should decode VMBC v13");
    assert!(
        program.code().windows(2).any(|pair| pair[0] == 0x1A),
        "compiler output should contain CallScript"
    );

    let mut vm = EmbeddedVm::new(program);
    assert_eq!(
        vm.run().expect("direct call should halt"),
        EmbeddedVmStatus::Halted
    );
    assert_eq!(vm.stack(), &[EmbeddedValue::Int(42)]);
}

#[test]
fn nostd_executes_std_encoded_result_ok_and_err_without_source() {
    let cases = [
        (
            "let value: Result<int, int> = Result::Ok(7); match value { Result::Ok(v) => v, Result::Err(e) => e };",
            EmbeddedValue::Int(7),
        ),
        (
            "let value: Result<int, int> = Result::Err(4); match value { Result::Ok(v) => v, Result::Err(e) => e };",
            EmbeddedValue::Int(4),
        ),
    ];
    for (source, expected) in cases {
        let compiled = compile_source(source).expect("Result source should compile");
        let bytes = encode_program(&compiled.program.with_local_count(compiled.locals))
            .expect("Result program should encode");
        assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), 13);
        let program = decode_program(&bytes).expect("no-std should decode v13 Result program");
        let mut vm = EmbeddedVm::new(program);
        assert_eq!(
            vm.run().expect("decoded Result program should halt"),
            EmbeddedVmStatus::Halted
        );
        assert_eq!(vm.stack().last().cloned(), Some(expected));
    }
}

#[test]
fn call_script_executes_nested_direct_calls() {
    let compiled = compile_source(
        "fn add2(value: int) -> int { value + 2 } fn add5(value: int) -> int { add2(value) + 3 } add5(0);",
    )
    .expect("nested call source should compile");
    let bytes = encode_program(&compiled.program.with_local_count(compiled.locals))
        .expect("nested call program should encode");
    let program = decode_program(&bytes).expect("no-std should decode nested call program");

    let mut vm = EmbeddedVm::new(program);
    assert_eq!(
        vm.run().expect("nested direct calls should halt"),
        EmbeddedVmStatus::Halted
    );
    assert_eq!(vm.stack(), &[EmbeddedValue::Int(5)]);
}

#[test]
fn call_script_recursion() {
    let compiled = compile_source(
        "fn fact(n: int) -> int { if n <= 1 => { 1 } else => { n * fact(n - 1) } } fact(10);",
    )
    .expect("recursion source should compile");
    let bytes = encode_program(&compiled.program.with_local_count(compiled.locals))
        .expect("recursion program should encode");
    let program = decode_program(&bytes).expect("no-std should decode recursion program");

    let mut vm = EmbeddedVm::new(program);
    assert_eq!(
        vm.run().expect("recursion should halt"),
        EmbeddedVmStatus::Halted
    );
    assert_eq!(vm.stack(), &[EmbeddedValue::Int(3_628_800)]);
}

#[test]
fn call_script_preserves_callee_local_isolation() {
    let compiled = compile_source(
        "fn set(value: int) -> int { let mut y = value; y = y + 1; y } let mut z = 10; z = set(z); z;",
    )
    .expect("local isolation source should compile");
    let bytes = encode_program(&compiled.program.with_local_count(compiled.locals))
        .expect("local isolation program should encode");
    let program = decode_program(&bytes).expect("no-std should decode local isolation program");

    let mut vm = EmbeddedVm::new(program);
    assert_eq!(
        vm.run().expect("local isolation should halt"),
        EmbeddedVmStatus::Halted
    );
    assert_eq!(vm.stack(), &[EmbeddedValue::Int(11)]);
}

#[test]
fn call_script_depth_limit() {
    let compiled =
        compile_source("fn f() -> int { f() } f();").expect("recursion source should compile");
    let bytes = encode_program(&compiled.program.with_local_count(compiled.locals))
        .expect("recursion program should encode");
    let program = decode_program(&bytes).expect("no-std should decode recursion program");

    let mut vm = EmbeddedVm::new(program);
    vm.set_max_script_call_depth(4)
        .expect("depth limit should be accepted");
    let err = vm
        .run()
        .expect_err("unbounded recursion should hit the depth limit");
    assert!(
        matches!(err, VmError::CallStackOverflow),
        "expected CallStackOverflow, got {err:?}"
    );
}

#[test]
fn call_script_capture_prototype_fails_typed() {
    // A script prototype that requires captures is wire-valid (runtime
    // concern), but `CallScript` can never supply an environment: the no-std
    // runtime must fail with the same typed error as the std interpreter.
    let code = vec![OpCode::CallScript as u8, 0, 0, 0, 0, 0];
    let program = raw_call_script_program(
        code,
        function_item_prototype(CallableTarget::ScriptFunction(0), 0, vec![0], None),
    );
    let bytes = encode_program(&program).expect("capture program should encode");
    let decoded = decode_program(&bytes).expect("no-std should decode capture program");

    let mut vm = EmbeddedVm::new(decoded);
    let err = vm
        .run()
        .expect_err("capture-requiring prototype should fail through CallScript");
    assert!(
        matches!(err, VmError::CallScriptRequiresEnvironment(0)),
        "expected CallScriptRequiresEnvironment(0), got {err:?}"
    );
}

#[test]
fn call_script_validation_rejects_out_of_range_prototype() {
    let code = vec![OpCode::CallScript as u8, 7, 0, 0, 0, 0];
    let program = raw_call_script_program(
        code,
        function_item_prototype(CallableTarget::ScriptFunction(0), 0, Vec::new(), None),
    );
    let bytes = encode_program(&program).expect("program should encode");
    let err = decode_program(&bytes).expect_err("out-of-range prototype should be rejected");
    assert!(
        matches!(err, WireError::InvalidCallScriptTarget { prototype_id: 7 }),
        "expected InvalidCallScriptTarget(7), got {err:?}"
    );
}

#[test]
fn call_script_validation_rejects_arity_mismatch() {
    let code = vec![OpCode::CallScript as u8, 0, 0, 0, 0, 1];
    let program = raw_call_script_program(
        code,
        function_item_prototype(CallableTarget::ScriptFunction(0), 0, Vec::new(), None),
    );
    let bytes = encode_program(&program).expect("program should encode");
    let err = decode_program(&bytes).expect_err("arity mismatch should be rejected");
    assert!(
        matches!(
            err,
            WireError::InvalidCallScriptArity {
                prototype_id: 0,
                expected: 0,
                got: 1
            }
        ),
        "expected InvalidCallScriptArity, got {err:?}"
    );
}

#[test]
fn call_script_validation_rejects_host_import_prototype() {
    let code = vec![OpCode::CallScript as u8, 0, 0, 0, 0, 0];
    let program = raw_call_script_program(
        code,
        function_item_prototype(CallableTarget::HostImport(0), 0, Vec::new(), None),
    );
    let bytes = encode_program(&program).expect("program should encode");
    let err = decode_program(&bytes).expect_err("host-import target should be rejected");
    assert!(
        matches!(err, WireError::InvalidCallScriptTarget { prototype_id: 0 }),
        "expected InvalidCallScriptTarget(0), got {err:?}"
    );
}

#[test]
fn call_script_validation_rejects_truncated_operands() {
    // 0x1A followed by only two operand bytes.
    let code = vec![OpCode::CallScript as u8, 1, 0];
    let program = raw_call_script_program(
        code,
        function_item_prototype(CallableTarget::ScriptFunction(0), 1, Vec::new(), None),
    );
    let bytes = encode_program(&program).expect("program should encode");
    let err = decode_program(&bytes).expect_err("truncated CallScript operands should be rejected");
    assert!(
        matches!(err, WireError::TruncatedOperand { .. }),
        "expected TruncatedOperand, got {err:?}"
    );
}

#[test]
fn call_script_rejects_v11_wire_version() {
    let compiled = compile_source("fn add2(value: int) -> int { value + 2 } add2(40);")
        .expect("direct call source should compile");
    let mut bytes = encode_program(&compiled.program.with_local_count(compiled.locals))
        .expect("direct call program should encode");
    bytes[4..6].copy_from_slice(&11u16.to_le_bytes());
    let err = decode_program(&bytes).expect_err("VMBC v11 must be rejected");
    assert!(
        matches!(err, WireError::UnsupportedVersion(11)),
        "expected UnsupportedVersion(11), got {err:?}"
    );
}

#[test]
fn call_script_fuel_interruption() {
    let compiled = compile_source(
        "fn bump(value: int) -> int { value + 1 } let mut i = 0; let mut total = 0; while i < 1000 { total = bump(total); i = i + 1; } total;",
    )
    .expect("fuel source should compile");
    let bytes = encode_program(&compiled.program.with_local_count(compiled.locals))
        .expect("fuel program should encode");
    let program = decode_program(&bytes).expect("no-std should decode fuel program");

    let mut vm = EmbeddedVm::new(program);
    vm.set_fuel(64);
    let err = vm
        .run()
        .expect_err("fuel should interrupt the direct call loop");
    assert!(
        matches!(err, VmError::OutOfFuel { .. }),
        "expected OutOfFuel, got {err:?}"
    );
}

#[test]
fn call_script_stack_underflow_precedes_environment_rejection() {
    // The interpreter checks operand underflow before prototype-driven
    // rejection: a malformed `CallScript` with argc > 0 and an empty stack
    // must report `StackUnderflow`, not `CallScriptRequiresEnvironment`,
    // even when the target prototype requires captures.
    let code = vec![OpCode::CallScript as u8, 0, 0, 0, 0, 1];
    let program = raw_call_script_program(
        code,
        function_item_prototype(CallableTarget::ScriptFunction(0), 1, vec![0], None),
    );
    let bytes = encode_program(&program).expect("program should encode");
    let decoded = decode_program(&bytes).expect("no-std should decode program");

    let mut vm = EmbeddedVm::new(decoded);
    let err = vm
        .run()
        .expect_err("short operand stack must fail with StackUnderflow");
    assert!(
        matches!(err, VmError::StackUnderflow),
        "expected StackUnderflow, got {err:?}"
    );
}

#[test]
fn call_script_binding_outside_frame_fails_typed() {
    // A root callable binding whose slot lies outside the callee frame is
    // invalid frame state: the no-std runtime must report the same typed
    // error as the std interpreter instead of silently skipping the slot.
    let mut code = vec![OpCode::CallScript as u8, 0, 0, 0, 0, 0];
    let function_entry = code.len() as u32;
    code.push(OpCode::Ret as u8);
    let function_end = code.len() as u32;
    let program = Program::new(Vec::new(), code)
        .with_local_count(2)
        .with_callable_metadata(
            vec![ScriptFunction {
                entry_ip: function_entry,
                end_ip: function_end,
            }],
            vec![function_item_prototype(
                CallableTarget::ScriptFunction(0),
                0,
                Vec::new(),
                None,
            )],
            vec![
                FunctionRegion {
                    start_ip: 0,
                    end_ip: function_entry,
                    prototype_id: None,
                },
                FunctionRegion {
                    start_ip: function_entry,
                    end_ip: function_end,
                    prototype_id: Some(0),
                },
            ],
            vec![vm::RootCallableBinding {
                local_slot: 1,
                prototype_id: 0,
            }],
        );
    let bytes = encode_program(&program).expect("program should encode");
    let decoded = decode_program(&bytes).expect("no-std should decode program");

    let mut vm = EmbeddedVm::new(decoded);
    let err = vm
        .run()
        .expect_err("out-of-frame root binding must fail on frame entry");
    assert!(
        matches!(
            err,
            VmError::InvalidFrameState("root callable binding is outside the script frame")
        ),
        "expected InvalidFrameState for the out-of-frame binding, got {err:?}"
    );
}

#[test]
fn nostd_executes_empty_cleanup_try_ok_and_err_without_source() {
    let ok_source = r#"
        fn pass() -> Result<int, int> { Result::Ok(7) }
        fn relay() -> Result<int, int> {
            let x: int = pass()?;
            Result::Ok(x + 1)
        }
        let value = match relay() {
            Result::Ok(v) => v,
            Result::Err(e) => e,
        };
        value;
    "#;
    let err_source = r#"
        fn fail() -> Result<int, int> { Result::Err(4) }
        fn relay() -> Result<int, int> {
            let x: int = fail()?;
            Result::Ok(x)
        }
        let value = match relay() {
            Result::Ok(v) => v,
            Result::Err(e) => e,
        };
        value;
    "#;
    for (source, expected) in [(ok_source, 8), (err_source, 4)] {
        let compiled = compile_source(source).expect("empty-cleanup try should compile");
        let bytes = encode_program(&compiled.program.with_local_count(compiled.locals))
            .expect("empty-cleanup try should encode");
        let decoded = decode_program(&bytes).expect("no-std should decode an empty cleanup plan");
        let mut vm = EmbeddedVm::new(decoded);
        assert_eq!(
            vm.run()
                .expect("empty-cleanup try should execute without source"),
            EmbeddedVmStatus::Halted
        );
        assert_eq!(vm.stack(), &[EmbeddedValue::Int(expected)]);
    }
}

#[test]
fn nostd_rejects_nonempty_resource_cleanup_without_executing() {
    use std::sync::Arc;
    use vm::{
        CompileSourceFileOptions, HostApiBuilder, HostFunctionSchema, HostTypeSchema,
        ResourceTypeKey, ResourceTypeSchema, SourceFlavor, compile_source_with_flavor_and_options,
    };

    let key = ResourceTypeKey::new("pr24.closure").expect("resource key");
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(key.clone(), "closure resource"));
    builder.function(HostFunctionSchema::with_return(
        "test::make_local",
        Vec::new(),
        HostTypeSchema::Resource(key.clone()),
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::make_err_result",
        Vec::new(),
        HostTypeSchema::Result {
            ok: Box::new(HostTypeSchema::Int),
            err: Box::new(HostTypeSchema::Resource(key)),
        },
    ));
    let catalog = Arc::new(builder.build().expect("catalog"));
    let source = r#"
        fn relay() -> Result<int, resource<pr24.closure>> {
            let local: resource<pr24.closure> = test::make_local();
            let value: int = test::make_err_result()?;
            Result::Ok(value)
        }
        let carried = relay();
        carried;
    "#;
    let compiled = compile_source_with_flavor_and_options(
        source,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::new().with_host_api_catalog(catalog),
    )
    .expect("resource try should compile");
    let bytes = encode_program(&compiled.program.with_local_count(compiled.locals))
        .expect("resource try should encode");
    let err = decode_program(&bytes).expect_err("no-std must reject a nonempty resource cleanup");
    assert!(
        matches!(
            err,
            WireError::UnsupportedCapability("ret cleanup resource plans")
        ),
        "expected an explicit capability rejection before execution, got {err:?}"
    );
}

fn resource_cleanup_bytes() -> Vec<u8> {
    use std::sync::Arc;
    use vm::{
        CompileSourceFileOptions, HostApiBuilder, HostFunctionSchema, HostTypeSchema,
        ResourceTypeKey, ResourceTypeSchema, SourceFlavor, compile_source_with_flavor_and_options,
    };
    let key = ResourceTypeKey::new("pr24.closure").expect("resource key");
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(key.clone(), "closure resource"));
    builder.function(HostFunctionSchema::with_return(
        "test::make_local",
        Vec::new(),
        HostTypeSchema::Resource(key.clone()),
    ));
    builder.function(HostFunctionSchema::with_return(
        "test::make_err_result",
        Vec::new(),
        HostTypeSchema::Result {
            ok: Box::new(HostTypeSchema::Int),
            err: Box::new(HostTypeSchema::Resource(key)),
        },
    ));
    let catalog = Arc::new(builder.build().expect("catalog"));
    let source = r#"
        fn relay() -> Result<int, resource<pr24.closure>> {
            let local: resource<pr24.closure> = test::make_local();
            let value: int = test::make_err_result()?;
            Result::Ok(value)
        }
        let carried = relay();
        carried;
    "#;
    let compiled = compile_source_with_flavor_and_options(
        source,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::new().with_host_api_catalog(catalog),
    )
    .expect("resource try should compile");
    encode_program(&compiled.program.with_local_count(compiled.locals)).expect("encode")
}

fn u32_le(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn replace_cleanup(bytes: &[u8], decls: &[u8], schema: &[u8]) -> Vec<u8> {
    let key = b"pr24.closure";
    let mut suffix = vec![17u8];
    suffix.extend_from_slice(&(key.len() as u32).to_le_bytes());
    suffix.extend_from_slice(key);
    assert!(
        bytes.ends_with(&suffix),
        "compiled cleanup should end in the resource schema, tail {:02x?}",
        &bytes[bytes.len().saturating_sub(48)..]
    );
    let schema_at = bytes.len() - suffix.len();
    let slot_at = schema_at - 2;
    let locals_at = slot_at - 4;
    let site_at = locals_at - 4;
    let plans_at = site_at - 4;
    let decls_at = plans_at - 4;
    assert_eq!(
        u32_le(bytes, decls_at),
        0,
        "fixture should have no named decls"
    );
    assert_eq!(u32_le(bytes, plans_at), 1);
    assert_eq!(u32_le(bytes, locals_at), 1);
    let site = &bytes[site_at..site_at + 4];
    let slot = &bytes[slot_at..schema_at];
    let mut out = bytes[..decls_at].to_vec();
    out.extend_from_slice(decls);
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(site);
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(slot);
    out.extend_from_slice(schema);
    out
}

fn named_decl(name: &str, params: &[&str], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&(name.len() as u32).to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(&(params.len() as u32).to_le_bytes());
    for param in params {
        out.extend_from_slice(&(param.len() as u32).to_le_bytes());
        out.extend_from_slice(param.as_bytes());
    }
    out.extend_from_slice(body);
    out
}

fn schema_resource() -> Vec<u8> {
    let key = b"pr24.closure";
    let mut out = vec![17u8];
    out.extend_from_slice(&(key.len() as u32).to_le_bytes());
    out.extend_from_slice(key);
    out
}

fn schema_named(name: &str, args: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![9u8];
    out.extend_from_slice(&(name.len() as u32).to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(&(args.len() as u32).to_le_bytes());
    for arg in args {
        out.extend_from_slice(arg);
    }
    out
}

fn schema_map(inner: &[u8]) -> Vec<u8> {
    let mut out = vec![13u8];
    out.extend_from_slice(inner);
    out
}

fn schema_result(ok: &[u8], err: &[u8]) -> Vec<u8> {
    let mut out = vec![18u8];
    out.extend_from_slice(ok);
    out.extend_from_slice(err);
    out
}

#[test]
fn nostd_named_validity_precedes_nonempty_capability_refusal() {
    let base = resource_cleanup_bytes();
    let body = schema_resource();
    let named = replace_cleanup(
        &base,
        &named_decl("Holder", &[], &body),
        &schema_named("Holder", &[]),
    );
    assert_eq!(
        decode_program(&named),
        Err(WireError::UnsupportedCapability(
            "ret cleanup resource plans"
        )),
        "a valid nongeneric resource body is a nonempty plan, not a missing resource"
    );
    let map = replace_cleanup(&base, &0u32.to_le_bytes(), &schema_map(&body));
    assert_eq!(
        decode_program(&map),
        Err(WireError::UnsupportedCapability(
            "ret cleanup resource plans"
        ))
    );
    let unresolved = replace_cleanup(
        &base,
        &0u32.to_le_bytes(),
        &schema_result(&body, &schema_named("Missing", &[])),
    );
    assert_eq!(
        decode_program(&unresolved),
        Err(WireError::InvalidRetCleanup(
            "unresolved named cleanup schema"
        ))
    );
    let unresolved_ok = replace_cleanup(
        &base,
        &0u32.to_le_bytes(),
        &schema_result(&schema_named("Missing", &[]), &body),
    );
    assert_eq!(
        decode_program(&unresolved_ok),
        Err(WireError::InvalidRetCleanup(
            "unresolved named cleanup schema"
        ))
    );
    let arity = replace_cleanup(
        &base,
        &named_decl("Holder", &["T"], &schema_resource()),
        &schema_named("Holder", &[]),
    );
    assert_eq!(
        decode_program(&arity),
        Err(WireError::InvalidRetCleanup("named cleanup schema arity"))
    );
    let cycle = replace_cleanup(
        &base,
        &named_decl("Holder", &[], &schema_named("Holder", &[])),
        &schema_named("Holder", &[]),
    );
    assert_eq!(
        decode_program(&cycle),
        Err(WireError::InvalidRetCleanup("named cleanup schema cycle"))
    );
}
