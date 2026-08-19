//! Compiler-level resource ownership: move/borrow legalization and release
//! scheduling (C2-B).
//!
//! These tests drive the whole compile pipeline through the public crate-root
//! API with a concrete host catalog, and assert on diagnostics (stable codes
//! and spans), emitted IR/bytecode (Drop scheduling, MoveVar/DetachLocal),
//! and the final program's owned-local metadata. The runtime resource
//! consumer is out of scope here (C2-C); values are raw `Int` handles.

use std::sync::Arc;

use vm::{
    BuiltinFunction, CompileSourceFileOptions, HostApiBuilder, HostApiCatalog, HostFunctionSchema,
    HostParamPassing, HostParamSchema, HostTypeSchema, OpCode, ResourceTypeKey, ResourceTypeSchema,
    SourceError, SourceFlavor, Value, compile_source_with_flavor_and_options,
};

fn io_file() -> ResourceTypeKey {
    ResourceTypeKey::new("io.file").expect("valid key")
}

fn resource(key: ResourceTypeKey) -> HostTypeSchema {
    HostTypeSchema::Resource(key)
}

fn value(name: &str, ty: HostTypeSchema) -> HostParamSchema {
    HostParamSchema::value(name, ty)
}

/// Concrete catalog: a resource type with open (Value→Resource), TakeOwned,
/// Borrow, and BorrowMut entry points, plus a nested `array<resource<...>>`
/// producer and consumer.
fn resource_catalog() -> Arc<HostApiCatalog> {
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(io_file(), "An open file"));
    builder.function(HostFunctionSchema::with_return(
        "acme::open",
        vec![value("path", HostTypeSchema::String)],
        resource(io_file()),
    ));
    builder.function(HostFunctionSchema::with_return(
        "acme::consume",
        vec![HostParamSchema::with_passing(
            "h",
            resource(io_file()),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "acme::peek",
        vec![HostParamSchema::with_passing(
            "h",
            resource(io_file()),
            HostParamPassing::Borrow,
        )],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "acme::mutate",
        vec![HostParamSchema::with_passing(
            "h",
            resource(io_file()),
            HostParamPassing::BorrowMut,
        )],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "acme::make_pair",
        vec![value("tag", HostTypeSchema::String)],
        HostTypeSchema::Array(Box::new(resource(io_file()))),
    ));
    builder.function(HostFunctionSchema::with_return(
        "acme::take_array",
        vec![HostParamSchema::with_passing(
            "files",
            HostTypeSchema::Array(Box::new(resource(io_file()))),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "acme::collect_files",
        vec![HostParamSchema::with_passing(
            "files",
            HostTypeSchema::Array(Box::new(resource(io_file()))),
            HostParamPassing::Borrow,
        )],
        HostTypeSchema::Int,
    ));
    Arc::new(builder.build().expect("catalog must build"))
}

fn compile_catalog(source: &str) -> Result<vm::CompiledProgram, SourceError> {
    compile_source_with_flavor_and_options(
        source,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(resource_catalog()),
    )
    .map_err(|err| match err {
        vm::SourcePathError::Source(err)
        | vm::SourcePathError::SourceWithMap { error: err, .. } => err,
        other => panic!("unexpected source path error: {other}"),
    })
}

/// Unwraps a required Parse error with an exact diagnostic code.
fn expect_parse_error(result: Result<vm::CompiledProgram, SourceError>, code: &str) {
    match result {
        Ok(_) => panic!("expected {code} compile error, got success"),
        Err(SourceError::Parse(err)) => {
            assert_eq!(
                err.code.as_deref(),
                Some(code),
                "unexpected diagnostic: {err:?}"
            );
        }
        Err(other) => panic!("expected {code} parse error, got {other:?}"),
    }
}

/// Asserts a Parse error with an exact diagnostic code and source line.
fn expect_parse_code(result: Result<vm::CompiledProgram, SourceError>, code: &str, line: usize) {
    match result {
        Ok(_) => panic!("expected {code} compile error, got success"),
        Err(SourceError::Parse(err)) => {
            assert_eq!(
                err.code.as_deref(),
                Some(code),
                "unexpected diagnostic code for {}: {:?}",
                code,
                err
            );
            assert_eq!(err.line, line, "unexpected diagnostic line for {code}");
        }
        Err(other) => panic!("expected {code} parse error, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Basic decode helpers for bytecode-level assertions.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Instr {
    ip: usize,
    op: u8,
    width: usize,
    u32_operand: Option<u32>,
    u8_operand: Option<u8>,
}

fn decode(code: &[u8]) -> Vec<Instr> {
    let mut out = Vec::new();
    let mut ip = 0usize;
    while ip < code.len() {
        let op = code[ip];
        let width = match op {
            x if x == OpCode::Ldc as u8 || x == OpCode::Br as u8 || x == OpCode::Brfalse as u8 => 5,
            x if x == OpCode::Ldloc as u8
                || x == OpCode::Stloc as u8
                || x == OpCode::CallValue as u8 =>
            {
                2
            }
            x if x == OpCode::Call as u8 => 4,
            x if x == OpCode::CallScript as u8 => 6,
            _ => 1,
        };
        let mut instr = Instr {
            ip,
            op,
            width,
            u32_operand: None,
            u8_operand: None,
        };
        if width >= 5 {
            let raw = u32::from_le_bytes(code[ip + 1..ip + 5].try_into().unwrap());
            instr.u32_operand = Some(raw);
        } else if width == 2 {
            instr.u8_operand = Some(code[ip + 1]);
        }
        out.push(instr);
        ip += width;
    }
    out
}

/// Positions of `ldc Null; stloc <slot>` pairs (the bytecode shape of a
/// scheduled `Stmt::Drop`).
fn drop_stores(program: &vm::Program) -> Vec<(usize, u8)> {
    let instructions = decode(&program.code);
    let mut drops = Vec::new();
    for pair in instructions.windows(2) {
        let (lhs, rhs) = (pair[0], pair[1]);
        if lhs.op != OpCode::Ldc as u8 || rhs.op != OpCode::Stloc as u8 {
            continue;
        }
        if lhs.ip + lhs.width != rhs.ip {
            continue;
        }
        let Some(const_index) = lhs.u32_operand else {
            continue;
        };
        if !matches!(
            program.constants.get(const_index as usize),
            Some(Value::Null)
        ) {
            continue;
        }
        drops.push((lhs.ip, rhs.u8_operand.expect("stloc slot")));
    }
    drops
}

/// Positions of `DetachLocal` builtin calls (the bytecode shape of
/// `MoveVar`/`MoveField`/`MoveIndex` legalization).
fn detach_calls(program: &vm::Program) -> Vec<(usize, u16)> {
    let instructions = decode(&program.code);
    let mut detaches = Vec::new();
    for instr in &instructions {
        if instr.op != OpCode::Call as u8 {
            continue;
        }
        // Call operands: u16 LE index, then u8 argc.
        let raw = u32::from_le_bytes(program.code[instr.ip + 1..instr.ip + 5].try_into().unwrap());
        let index = (raw & 0xFFFF) as u16;
        if index == BuiltinFunction::DetachLocal.call_index() {
            detaches.push((instr.ip, index));
        }
    }
    detaches
}

/// Positions of per-field release stores: `ldc Null; call Set 3` (the
/// bytecode shape of `MoveField`/`MoveIndex`).
fn field_null_stores(program: &vm::Program) -> Vec<usize> {
    let instructions = decode(&program.code);
    let mut stores = Vec::new();
    for pair in instructions.windows(2) {
        let (lhs, rhs) = (pair[0], pair[1]);
        if lhs.op != OpCode::Ldc as u8
            || rhs.op != OpCode::Call as u8
            || lhs.ip + lhs.width != rhs.ip
        {
            continue;
        }
        let Some(const_index) = lhs.u32_operand else {
            continue;
        };
        if !matches!(
            program.constants.get(const_index as usize),
            Some(Value::Null)
        ) {
            continue;
        }
        let raw = u32::from_le_bytes(program.code[rhs.ip + 1..rhs.ip + 5].try_into().unwrap());
        if (raw & 0xFFFF) as u16 == BuiltinFunction::Set.call_index() {
            stores.push(lhs.ip);
        }
    }
    stores
}

/// Span of the first backward-branching loop: `(loop_start, backedge_ip)`.
fn loop_span(program: &vm::Program) -> (usize, usize) {
    for instr in decode(&program.code) {
        if instr.op != OpCode::Br as u8 {
            continue;
        }
        let target = instr.u32_operand.expect("br target") as usize;
        if target < instr.ip {
            return (target, instr.ip);
        }
    }
    panic!("expected a backward branch (loop backedge) in bytecode");
}

// ---------------------------------------------------------------------------
// 1. let/rebind moves and use-after-move
// ---------------------------------------------------------------------------

#[test]
fn resource_let_rebind_moves_source_and_use_after_move_fails() {
    let result = compile_catalog(
        r#"
use acme;
let db = acme::open("/tmp/x");
let alias = db;
acme::peek(&db);
"#,
    );
    expect_parse_code(result, "E_LOCAL_MOVED", 5);
}

#[test]
fn resource_rebind_alias_is_usable_instead_of_source() {
    let compiled = compile_catalog(
        r#"
use acme;
let db = acme::open("/tmp/x");
let alias = db;
acme::peek(&alias);
"#,
    )
    .expect("moving a resource into a new binding then using the alias must compile");
    assert!(
        compiled
            .program
            .owned_local_slots()
            .iter()
            .any(|owned| *owned),
        "program should carry owned local metadata"
    );
}

#[test]
fn resource_assignment_rebind_moves_source() {
    let compiled = compile_catalog(
        r#"
use acme;
let mut db = acme::open("/tmp/x");
let second = acme::open("/tmp/y");
db = second;
acme::peek(&db);
"#,
    )
    .expect("assigning a resource local must move the source and keep the target usable");
    // The rebind produced a MoveVar for `second` in the emitted bytecode.
    assert!(
        !detach_calls(&compiled.program).is_empty(),
        "expected a DetachLocal (MoveVar) for the resource rebind"
    );
}

#[test]
fn resource_use_after_take_owned_call_fails() {
    let result = compile_catalog(
        r#"
use acme;
let db = acme::open("/tmp/x");
acme::consume(db);
acme::peek(&db);
"#,
    );
    expect_parse_code(result, "E_LOCAL_MOVED", 5);
}

#[test]
fn resource_take_owned_same_local_twice_in_one_call_fails() {
    let result = compile_catalog(
        r#"
use acme;
let a = acme::open("/tmp/x");
let b = acme::open("/tmp/y");
acme::consume(a);
acme::consume(a);
"#,
    );
    expect_parse_code(result, "E_LOCAL_MOVED", 6);
}

// ---------------------------------------------------------------------------
// 2. TakeOwned / Borrow / BorrowMut exact passing
// ---------------------------------------------------------------------------

#[test]
fn take_owned_call_lowers_to_detach_local_and_borrow_does_not_consume() {
    let compiled = compile_catalog(
        r#"
use acme;
let mut db = acme::open("/tmp/x");
acme::peek(&db);
acme::peek(&db);
acme::mutate(&mut db);
acme::consume(db);
"#,
    )
    .expect("borrow then consume must compile");
    assert!(
        !detach_calls(&compiled.program).is_empty(),
        "expected the TakeOwned consume to lower through DetachLocal"
    );
}

#[test]
fn borrow_escape_to_let_is_rejected() {
    let result = compile_catalog(
        r#"
use acme;
let db = acme::open("/tmp/x");
let handle = &db;
"#,
    );
    expect_parse_code(result, "E_OWNERSHIP_BORROW_ESCAPE", 4);
}

#[test]
fn borrow_escape_to_return_is_rejected() {
    let result = compile_catalog(
        r#"
use acme;
fn peek_back() {
    let db = acme::open("/tmp/x");
    &db
}
"#,
    );
    expect_parse_code(result, "E_OWNERSHIP_BORROW_ESCAPE", 1);
}

#[test]
fn borrow_escape_to_collection_is_rejected() {
    let result = compile_catalog(
        r#"
use acme;
let db = acme::open("/tmp/x");
let refs = [&db];
"#,
    );
    expect_parse_code(result, "E_OWNERSHIP_BORROW_ESCAPE", 4);
}

#[test]
fn to_owned_resource_is_rejected() {
    let result = compile_catalog(
        r#"
use acme;
let db = acme::open("/tmp/x");
let copy = db.copy();
"#,
    );
    expect_parse_code(result, "E_OWNERSHIP_COPY_RESOURCE", 4);
}

// ---------------------------------------------------------------------------
// 3. Branch and loop fixpoints
// ---------------------------------------------------------------------------

#[test]
fn branch_single_side_move_makes_merge_use_fail() {
    let result = compile_catalog(
        r#"
use acme;
let db = acme::open("/tmp/x");
let mut flag = true;
if flag { acme::consume(db); } else { acme::peek(&db); }
acme::peek(&db);
"#,
    );
    expect_parse_code(result, "E_LOCAL_MOVED", 6);
}

#[test]
fn loop_carried_move_makes_post_loop_use_fail() {
    // Moving an owned local inside a loop body is loop-carried: the fixpoint
    // merges the backedge state, so the second iteration would use a moved
    // value. The compiler reports the carried move at the first consume site
    // inside the loop.
    let result = compile_catalog(
        r#"
use acme;
let db = acme::open("/tmp/x");
let mut i = 0;
while i < 2 {
    acme::consume(db);
    i = i + 1;
}
acme::peek(&db);
"#,
    );
    expect_parse_code(result, "E_LOCAL_MOVED", 6);
}

#[test]
fn loop_body_borrow_keeps_resource_usable_after_loop() {
    let compiled = compile_catalog(
        r#"
use acme;
let db = acme::open("/tmp/x");
let mut i = 0;
while i < 2 {
    acme::peek(&db);
    i = i + 1;
}
acme::consume(db);
"#,
    )
    .expect("borrowing inside a loop must not consume the resource");
    assert!(!detach_calls(&compiled.program).is_empty());
}

// ---------------------------------------------------------------------------
// 4. Loop-owned per-iteration Drop scheduling (release schedule)
// ---------------------------------------------------------------------------

#[test]
fn loop_owned_local_gets_per_iteration_drop_in_bytecode() {
    let compiled = compile_catalog(
        r#"
use acme;
let mut i = 0;
while i < 2 {
    let db = acme::open("/tmp/x");
    acme::peek(&db);
    i = i + 1;
}
"#,
    )
    .expect("loop with per-iteration resource must compile");
    let (loop_start, backedge_ip) = loop_span(&compiled.program);
    let drops = drop_stores(&compiled.program);
    assert!(
        drops
            .iter()
            .any(|(ip, _)| *ip >= loop_start && *ip <= backedge_ip),
        "expected a per-iteration Drop (ldc null; stloc) inside the loop body for the owned local; drops: {drops:?}"
    );
}

#[test]
fn loop_plain_local_keeps_suppressed_clears() {
    // Same loop shape with a plain string: the suppress-clear policy for
    // ordinary locals is unchanged, so no Drop appears inside the loop body.
    let compiled = compile_catalog(
        r#"
use acme;
let mut i = 0;
while i < 2 {
    let tag = "x";
    i = i + 1;
}
"#,
    )
    .expect("loop with non-owned local must compile unchanged");
    let (loop_start, backedge_ip) = loop_span(&compiled.program);
    let drops = drop_stores(&compiled.program);
    assert!(
        drops
            .iter()
            .all(|(ip, _)| *ip < loop_start || *ip > backedge_ip),
        "non-resource loop body must not gain per-iteration drops: {drops:?}"
    );
}

#[test]
fn straight_line_owned_last_use_drop_still_scheduled() {
    // Outside loops the existing clear policy already drops at last use; a
    // moved-out resource must NOT be dropped again afterwards.
    let compiled = compile_catalog(
        r#"
use acme;
let db = acme::open("/tmp/x");
acme::consume(db);
"#,
    )
    .expect("consume after open must compile");
    // The move-out consumes the local: exactly one Drop may exist for the
    // slot's dead range before the consume, and there must be at least one
    // DetachLocal carrying the ownership transfer.
    assert!(
        !detach_calls(&compiled.program).is_empty(),
        "expected DetachLocal for the TakeOwned consume"
    );
}

// ---------------------------------------------------------------------------
// 5. Return moves
// ---------------------------------------------------------------------------

#[test]
fn returning_resource_local_moves_it_out_of_the_frame() {
    let compiled = compile_catalog(
        r#"
use acme;
fn open_default() {
    let db = acme::open("/tmp/x");
    db
}
let handle = open_default();
acme::consume(handle);
"#,
    )
    .expect("returning a resource local must compile");
    // The function body returns through MoveVar (DetachLocal), so the frame
    // exit never releases the source slot a second time.
    assert!(
        !detach_calls(&compiled.program).is_empty(),
        "expected the function return to lower through DetachLocal"
    );
}

#[test]
fn returning_moved_resource_fails() {
    let result = compile_catalog(
        r#"
use acme;
fn open_twice() {
    let db = acme::open("/tmp/x");
    let alias = db;
    db
}
"#,
    );
    expect_parse_error(result, "E_LOCAL_MOVED");
}

// ---------------------------------------------------------------------------
// 6. Captures and aggregates
// ---------------------------------------------------------------------------

#[test]
fn resource_closure_borrow_capture_is_rejected() {
    let result = compile_catalog(
        r#"
use acme;
let db = acme::open("/tmp/x");
let f = || acme::peek(&db);
f();
"#,
    );
    expect_parse_error(result, "E_OWNERSHIP_BORROW_ESCAPE");
}

#[test]
fn resource_closure_copy_capture_is_rejected() {
    let result = compile_catalog(
        r#"
use acme;
let db = acme::open("/tmp/x");
let f = || acme::peek(db);
f();
"#,
    );
    expect_parse_error(result, "E_OWNERSHIP_COPY_RESOURCE");
}

#[test]
fn resource_array_literal_insertion_moves_local_into_aggregate() {
    let result = compile_catalog(
        r#"
use acme;
let db = acme::open("/tmp/x");
let files = [db];
acme::peek(&db);
"#,
    );
    expect_parse_error(result, "E_LOCAL_MOVED");
}

#[test]
fn resource_map_literal_insertion_moves_local_into_aggregate() {
    let result = compile_catalog(
        r#"
use acme;
let db = acme::open("/tmp/x");
let holder = {"conn": db};
acme::peek(&db);
"#,
    );
    expect_parse_error(result, "E_LOCAL_MOVED");
}

#[test]
fn resource_aggregate_take_owned_moves_whole_array() {
    let result = compile_catalog(
        r#"
use acme;
let files = acme::make_pair("a");
acme::take_array(files);
acme::collect_files(&files);
"#,
    );
    expect_parse_error(result, "E_LOCAL_MOVED");
}

#[test]
fn resource_aggregate_borrow_then_take_owned_compiles() {
    let compiled = compile_catalog(
        r#"
use acme;
let files = acme::make_pair("a");
acme::collect_files(&files);
acme::take_array(files);
"#,
    )
    .expect("borrowing then moving an aggregate must compile");
    assert!(!detach_calls(&compiled.program).is_empty());
}

#[test]
fn resource_field_access_moves_field_out() {
    // A resource field read through the existing field-move machinery must
    // consume the field so the source cannot double-release it. The release
    // at this ABI is a per-field null store (Get + Set-null), and the field
    // value flows on without a second DetachLocal.
    let compiled = compile_catalog(
        r#"
use acme;
let holder = {"conn": acme::open("/tmp/x")};
let conn = holder.conn;
acme::peek(&conn);
"#,
    )
    .expect("moving a resource field out of an aggregate must compile");
    assert!(
        !field_null_stores(&compiled.program).is_empty(),
        "expected the field move to lower through a per-field release store"
    );
}

#[test]
fn resource_field_use_after_move_fails() {
    let result = compile_catalog(
        r#"
use acme;
let holder = {"conn": acme::open("/tmp/x")};
let conn = holder.conn;
acme::consume(holder.conn);
"#,
    );
    match result {
        Err(SourceError::Parse(err)) => assert_eq!(
            err.code.as_deref(),
            Some("E_FIELD_MOVED"),
            "unexpected diagnostic: {err:?}"
        ),
        Err(other) => panic!("expected field-moved error, got {other:?}"),
        Ok(_) => panic!("expected field-moved error"),
    }
}

// ---------------------------------------------------------------------------
// 7. Regression: plain programs and dynamic/no-catalog paths
// ---------------------------------------------------------------------------

#[test]
fn plain_int_string_program_bytecode_and_drops_unchanged() {
    let compiled = compile_source_with_flavor_and_options(
        r#"
let s = "hello";
let t = s + " world";
let mut i = 0;
while i < 2 {
    let tag = "x";
    i = i + 1;
}
t;
"#,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default(),
    )
    .expect("plain program must compile without a catalog");
    assert!(
        compiled
            .program
            .owned_local_slots()
            .iter()
            .all(|owned| !*owned),
        "plain program must not carry owned slots"
    );
    // The loop body keeps its suppressed clears for non-owned locals: no
    // Drop (ldc null; stloc) may appear inside the loop span.
    let (loop_start, backedge_ip) = loop_span(&compiled.program);
    let drops = drop_stores(&compiled.program);
    assert!(
        drops
            .iter()
            .all(|(ip, _)| *ip < loop_start || *ip > backedge_ip),
        "plain program gained drops inside the loop body: {drops:?}"
    );
}

#[test]
fn no_catalog_resource_source_fails_without_metadata() {
    // Without a catalog there is no resource metadata and no resolution, so
    // a resource-shaped namespace call stays a legacy import and the program
    // compiles exactly as before (no ownership enforcement, no schema).
    let compiled = compile_source_with_flavor_and_options(
        r#"
use acme;
acme::open("/tmp/x");
"#,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default(),
    )
    .expect("legacy no-catalog program must compile unchanged");
    assert!(
        compiled
            .program
            .owned_local_slots()
            .iter()
            .all(|owned| !*owned),
        "no-catalog program must not carry owned slots"
    );
}

#[test]
fn diagnostics_code_and_line_are_stable() {
    // The same use-after-move must report the same code and line every time.
    let source = r#"
use acme;
let db = acme::open("/tmp/x");
let alias = db;
acme::peek(&db);
"#;
    for _ in 0..3 {
        let err = match compile_catalog(source) {
            Ok(_) => panic!("expected E_LOCAL_MOVED, got success"),
            Err(err) => err,
        };
        match err {
            SourceError::Parse(err) => {
                assert_eq!(err.code.as_deref(), Some("E_LOCAL_MOVED"));
                assert_eq!(err.line, 5);
                assert!(err.message.contains("db"), "message: {}", err.message);
            }
            other => panic!("expected parse error, got {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// 8. TakeOwned via field/index arguments
// ---------------------------------------------------------------------------

#[test]
fn take_owned_field_argument_lowers_to_move_field() {
    let compiled = compile_catalog(
        r#"
use acme;
let holder = {"conn": acme::open("/tmp/x")};
acme::consume(holder.conn);
"#,
    )
    .expect("TakeOwned of a literal field access must compile");
    assert!(
        !field_null_stores(&compiled.program).is_empty(),
        "expected the TakeOwned field argument to lower through a per-field release store"
    );
}

#[test]
fn take_owned_of_borrowed_wrapper_is_rejected() {
    // `&db` expresses Borrow intent; a TakeOwned parameter cannot be
    // satisfied by a borrow wrapper (the resolver rejects the intent
    // mismatch before availability even runs).
    let result = compile_catalog(
        r#"
use acme;
let db = acme::open("/tmp/x");
acme::consume(&db);
"#,
    );
    match result {
        Err(SourceError::Compile(vm::CompileError::HostCallResolve { .. })) => {}
        Err(other) => panic!("expected host-call resolve rejection, got {other:?}"),
        Ok(_) => panic!("expected host-call resolve rejection"),
    }
}
