use std::sync::Arc;

use vm::compiler::{CompileSourceFileOptions, SourceFlavor, TypeSchema};
use vm::{
    CallOutcome, CallReturn, HostApiBuilder, HostApiCatalog, HostApiFingerprint, HostFunction,
    HostFunctionRegistry, HostFunctionSchema, HostImport, HostImportBindingError, HostImportParam,
    HostImportSchema, HostParamPassing, HostParamSchema, HostTypeSchema, Value, ValueType, Vm,
    VmError, VmResult, VmStatus, compile_source_with_flavor_and_options,
};

/// Concrete host fn that answers a fixed Int tag (ignores its argument).
#[derive(Clone, Copy)]
struct Tag(i64);

impl HostFunction for Tag {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        Ok(CallOutcome::Return(CallReturn::One(Value::Int(self.0))))
    }
}

fn tag_factory(tag: i64) -> impl Fn() -> Box<dyn HostFunction> + Send + Sync + 'static {
    move || Box::new(Tag(tag))
}

fn build_catalog(functions: Vec<HostFunctionSchema>) -> Arc<HostApiCatalog> {
    let mut builder = HostApiBuilder::new();
    for function in functions {
        builder.function(function);
    }
    Arc::new(builder.build().expect("catalog must build"))
}

/// A single-`Int`-param, `Int`-return host function schema.
fn int_fn() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        "x::f",
        vec![HostParamSchema::value("value", HostTypeSchema::Int)],
        HostTypeSchema::Int,
    )
}

/// Real catalog fingerprint for `int_fn()`. When `extra` is true an unrelated function is
/// added so the *catalog-level* fingerprint differs while `int_fn`'s own schema stays
/// identical — mirroring a real catalog-version change.
fn int_schema_fingerprint(extra: bool) -> HostApiFingerprint {
    let functions = if extra {
        vec![
            int_fn(),
            HostFunctionSchema::with_return(
                "extra::other",
                vec![HostParamSchema::value("s", HostTypeSchema::String)],
                HostTypeSchema::String,
            ),
        ]
    } else {
        vec![int_fn()]
    };
    build_catalog(functions).fingerprint()
}

/// Exact schema for one `Int` `Value` param returning `Int`, carrying a real catalog fingerprint.
fn int_exact_schema(fingerprint: HostApiFingerprint) -> HostImportSchema {
    HostImportSchema {
        params: vec![HostImportParam {
            name: "value".into(),
            schema: TypeSchema::Int,
            passing: HostParamPassing::Value,
        }],
        return_type: TypeSchema::Int,
        fingerprint,
    }
}

/// Catalog with two same-name overloads differing by *argument* exact schema:
/// `acme::compute(int) -> Int` and `acme::compute(bool) -> Int`.
fn catalog() -> Arc<HostApiCatalog> {
    let mut builder = HostApiBuilder::new();
    builder.function(HostFunctionSchema::with_return(
        "acme::compute",
        vec![HostParamSchema::value("x", HostTypeSchema::Int)],
        HostTypeSchema::Int,
    ));
    builder.function(HostFunctionSchema::with_return(
        "acme::compute",
        vec![HostParamSchema::value("x", HostTypeSchema::Bool)],
        HostTypeSchema::Int,
    ));
    Arc::new(builder.build().expect("catalog must build"))
}

/// (1) Same name, two distinct exact schemas from the catalog → two distinct slots;
/// each import dispatches to the distinct host fn it was exact-bound to.
#[test]
fn same_name_distinct_exact_schemas_resolve_to_separate_slots() {
    let compiled = compile_source_with_flavor_and_options(
        r#"
use acme;
acme::compute(1);
acme::compute(true);
"#,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(catalog()),
    )
    .expect("catalog source should compile");

    let computes = compiled
        .program
        .imports
        .iter()
        .filter(|i| i.name == "acme::compute")
        .map(|i| i.schema.as_ref().expect("resolved exact schema").clone())
        .collect::<Vec<_>>();
    assert_eq!(computes.len(), 2, "two exact compute overloads");
    assert_ne!(
        computes[0], computes[1],
        "the two overloads differ in schema"
    );

    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact("acme::compute", 1, computes[0].clone(), tag_factory(100))
        .expect("bind int overload");
    registry
        .register_exact("acme::compute", 1, computes[1].clone(), tag_factory(200))
        .expect("bind bool overload");

    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    registry
        .bind_vm_cached(&mut vm)
        .expect("exact bind should succeed");

    assert_eq!(vm.run().expect("run"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(100), Value::Int(200)]);
}

/// (2) Fingerprint differs (same param/return schema, different real catalog fingerprint)
/// → `resolve_import` with an unmatched schema rejects and never falls back to the
/// legacy by-name slot.
#[test]
fn fingerprint_mismatch_rejected_without_by_name_fallback() {
    let fp_a = int_schema_fingerprint(false);
    let fp_b = int_schema_fingerprint(true);
    assert_ne!(
        fp_a, fp_b,
        "catalog fingerprints must differ with the extra function"
    );

    let schema_a = int_exact_schema(fp_a);
    let schema_b = HostImportSchema {
        fingerprint: fp_b,
        ..schema_a.clone()
    };

    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact("echo::emit", 1, schema_a, tag_factory(5))
        .unwrap();
    // Also register a legacy by-name slot to prove there is no fallback.
    registry.register("echo::emit", 1, tag_factory(9));

    let import = HostImport {
        name: "echo::emit".into(),
        arity: 1,
        return_type: ValueType::Int,
        schema: Some(schema_b),
    };
    let error = registry
        .resolve_import(&import)
        .expect_err("fingerprint mismatch must be rejected");
    assert!(
        matches!(
            error,
            VmError::HostImportBinding(HostImportBindingError::MissingExact { ref import })
                if import == "echo::emit"
        ),
        "expected structured MissingExact, got: {error}"
    );
}

/// (3) Param passing mismatch (exact `Value` vs import `BorrowMut`) → rejected.
#[test]
fn param_passing_schema_mismatch_rejected() {
    let fp = int_schema_fingerprint(false);
    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact("io::read", 1, int_exact_schema(fp), tag_factory(7))
        .unwrap();

    let import = HostImport {
        name: "io::read".into(),
        arity: 1,
        return_type: ValueType::Int,
        schema: Some(HostImportSchema {
            params: vec![HostImportParam {
                name: "value".into(),
                schema: TypeSchema::Int,
                passing: HostParamPassing::BorrowMut,
            }],
            return_type: TypeSchema::Int,
            fingerprint: fp,
        }),
    };
    let error = registry
        .resolve_import(&import)
        .expect_err("param passing mismatch must fail");
    assert!(
        matches!(
            error,
            VmError::HostImportBinding(HostImportBindingError::MissingExact { .. })
        ),
        "expected structured MissingExact, got: {error}"
    );
}

/// (4) Return-schema mismatch (exact Int vs import Bool return) → structured rejection.
#[test]
fn exact_return_schema_mismatch_rejected() {
    let fp = int_schema_fingerprint(false);
    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact("io::read", 1, int_exact_schema(fp), tag_factory(8))
        .unwrap();

    let import = HostImport {
        name: "io::read".into(),
        arity: 1,
        return_type: ValueType::Bool,
        schema: Some(int_exact_schema(fp)),
    };
    let error = registry
        .resolve_import(&import)
        .expect_err("return schema mismatch must fail");
    assert!(
        matches!(
            error,
            VmError::HostImportBinding(HostImportBindingError::ReturnTypeMismatch {
                ref import,
                expected,
                got,
            }) if import == "io::read" && expected == ValueType::Int && got == ValueType::Bool
        ),
        "expected structured ReturnTypeMismatch, got: {error}"
    );
}

/// (5) A legacy `schema:None` name-only binding cannot hijack an existing exact slot:
/// the exact binding still resolves to its own slot and its own function.
#[test]
fn legacy_name_only_binding_cannot_hijack_exact_slot() {
    let fp = int_schema_fingerprint(false);
    let mut registry = HostFunctionRegistry::new();
    let slot_exact = registry
        .register_exact("srv::ping", 1, int_exact_schema(fp), tag_factory(5))
        .unwrap();
    registry.register("srv::ping", 1, tag_factory(9));

    let import = HostImport {
        name: "srv::ping".into(),
        arity: 1,
        return_type: ValueType::Int,
        schema: Some(int_exact_schema(fp)),
    };
    let slot = registry
        .resolve_import(&import)
        .expect("exact binding must win");
    assert_eq!(slot, slot_exact, "exact slot must not be hijacked");
}

/// (6) Duplicate exact (name+schema) registration → structured deterministic error,
/// with no registry mutation (cache unchanged, original slot still resolves).
#[test]
fn duplicate_exact_registration_rejected() {
    let fp = int_schema_fingerprint(false);
    let schema = int_exact_schema(fp);
    let mut registry = HostFunctionRegistry::new();
    let slot = registry
        .register_exact("io::read", 1, schema.clone(), tag_factory(1))
        .unwrap();
    let cache_before = registry.plan_cache_len();
    let err = registry
        .register_exact("io::read", 1, schema.clone(), tag_factory(2))
        .expect_err("duplicate exact registration must error");
    assert!(
        matches!(
            err,
            VmError::HostImportBinding(HostImportBindingError::Duplicate { ref import })
                if import == "io::read"
        ),
        "expected structured Duplicate, got: {err}"
    );
    assert_eq!(
        registry.plan_cache_len(),
        cache_before,
        "failed duplicate registration must not touch the plan cache"
    );
    let import = HostImport {
        name: "io::read".into(),
        arity: 1,
        return_type: ValueType::Int,
        schema: Some(schema),
    };
    assert_eq!(
        registry.resolve_import(&import).unwrap(),
        slot,
        "original exact slot must survive a rejected duplicate"
    );
}

/// (7) Registration-time arity vs. schema-parameter-count mismatch → structured error,
/// and the failed registration is atomic (no cache change, no slot created).
#[test]
fn exact_registration_arity_mismatch_is_structured_and_atomic() {
    let fp = int_schema_fingerprint(false);
    let schema = int_exact_schema(fp);
    let mut registry = HostFunctionRegistry::new();
    let slot_ok = registry
        .register_exact("x::f", 1, schema.clone(), tag_factory(1))
        .unwrap();
    let cache_before = registry.plan_cache_len();

    let err = registry
        .register_exact("x::f", 2, schema.clone(), tag_factory(2))
        .expect_err("arity mismatch must be rejected at registration");
    assert!(
        matches!(
            err,
            VmError::HostImportBinding(HostImportBindingError::SchemaArityMismatch {
                ref import,
                expected,
                got,
            }) if import == "x::f" && expected == 1 && got == 2
        ),
        "expected structured SchemaArityMismatch, got: {err}"
    );
    assert_eq!(
        registry.plan_cache_len(),
        cache_before,
        "failed registration must not touch the plan cache"
    );

    let import = HostImport {
        name: "x::f".into(),
        arity: 1,
        return_type: ValueType::Int,
        schema: Some(schema),
    };
    assert_eq!(
        registry.resolve_import(&import).unwrap(),
        slot_ok,
        "original slot must be unchanged after a rejected registration"
    );
}

/// (8) A `TypeSchema::Number` return (and `Optional<Number>`) is *legal* for exact
/// registration: registering succeeds; the exact name+schema resolves; and binding
/// yields the registered host tag. (The registration-time coarse-`Unknown` rejection was
/// removed, so consistency is verified at bind time instead.)
#[test]
fn exact_number_schema_registers_resolves_and_binds() {
    let catalog = build_catalog(
        [HostFunctionSchema::with_return(
            "calc::num",
            vec![HostParamSchema::value("n", HostTypeSchema::Number)],
            HostTypeSchema::Number,
        )]
        .to_vec(),
    );

    let compiled = compile_source_with_flavor_and_options(
        r#"use calc; calc::num(1);"#,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(catalog),
    )
    .expect("catalog source must compile");

    // The compiler resolves the host call against the real catalog and materialises the
    // exact import (name + schema + real fingerprint). Register the exact binding from
    // that compiled artifact, then bind the same program.
    let import = compiled
        .program
        .imports
        .iter()
        .find(|i| i.name == "calc::num")
        .expect("compiled program must carry the calc::num import");
    assert_eq!(
        import.return_type,
        ValueType::Unknown,
        "TypeSchema::Number coarse value type"
    );
    let schema = import.schema.clone().expect("resolved exact schema");

    let mut registry = HostFunctionRegistry::new();
    let slot = registry
        .register_exact("calc::num", 1, schema.clone(), tag_factory(7))
        .expect("exact Number-returning schema must now register");

    // Exact schema match: resolves to the freshly registered slot.
    assert_eq!(registry.resolve_import(import).unwrap(), slot);

    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    registry
        .bind_vm_cached(&mut vm)
        .expect("exact bind must succeed");
    assert_eq!(vm.run().expect("run"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(7)]);
}

/// (14) The same is legal for `Optional<Number>`: exact registration on the
/// `Optional(Number)` return schema registers, resolves by that exact schema, and
/// binds to yield the registered tag.
#[test]
fn exact_optional_number_schema_registers_resolves_and_binds() {
    let catalog = build_catalog(
        [HostFunctionSchema::with_return(
            "calc::opt",
            vec![HostParamSchema::value("n", HostTypeSchema::Number)],
            HostTypeSchema::Optional(Box::new(HostTypeSchema::Number)),
        )]
        .to_vec(),
    );

    let compiled = compile_source_with_flavor_and_options(
        r#"use calc; calc::opt(1);"#,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::default().with_host_api_catalog(catalog),
    )
    .expect("catalog source must compile");

    let import = compiled
        .program
        .imports
        .iter()
        .find(|i| i.name == "calc::opt")
        .expect("compiled program must carry the calc::opt import");
    assert_eq!(
        import.return_type,
        ValueType::Unknown,
        "Optional<Number> coarse value type"
    );
    let schema = import.schema.clone().expect("resolved exact schema");

    let mut registry = HostFunctionRegistry::new();
    let slot = registry
        .register_exact("calc::opt", 1, schema.clone(), tag_factory(21))
        .expect("exact Optional-Number-returning schema must now register");

    // Exact schema match: resolves to the just-registered slot.
    assert_eq!(registry.resolve_import(import).unwrap(), slot);

    let mut vm = Vm::try_new(compiled.program).expect("test VM construction must not fail");
    registry
        .bind_vm_cached(&mut vm)
        .expect("exact bind must succeed");
    assert_eq!(vm.run().expect("run"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(21)]);
}

/// (9) Plan cache partitions by exact schema: same name & arity, different exact schemas
/// produce separate cache entries / distinct import signatures.
#[test]
fn plan_cache_partitions_by_exact_schema() {
    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact(
            "calc::m",
            1,
            int_exact_schema(int_schema_fingerprint(false)),
            tag_factory(1),
        )
        .unwrap();
    registry
        .register_exact(
            "calc::m",
            1,
            int_exact_schema(int_schema_fingerprint(true)),
            tag_factory(2),
        )
        .unwrap();

    let imports_1 = [HostImport {
        name: "calc::m".into(),
        arity: 1,
        return_type: ValueType::Int,
        schema: Some(int_exact_schema(int_schema_fingerprint(false))),
    }];
    let before = registry.plan_cache_len();
    let plan_1 = registry.prepare_plan(&imports_1).unwrap();
    assert_eq!(before + 1, registry.plan_cache_len());

    let imports_2 = [HostImport {
        name: "calc::m".into(),
        arity: 1,
        return_type: ValueType::Int,
        schema: Some(int_exact_schema(int_schema_fingerprint(true))),
    }];
    let plan_2 = registry.prepare_plan(&imports_2).unwrap();
    assert_eq!(
        before + 2,
        registry.plan_cache_len(),
        "different exact schema needs its own cache entry"
    );

    assert_ne!(plan_1.import_signature(), plan_2.import_signature());
}

/// (10) `schema: None` legacy static import path keeps working independently.
#[test]
fn legacy_schema_none_import_path_is_preserved() {
    let mut registry = HostFunctionRegistry::new();
    registry.register("legacy::echo", 1, tag_factory(99));

    let import = HostImport {
        name: "legacy::echo".into(),
        arity: 1,
        return_type: ValueType::Int,
        schema: None,
    };
    let _ = registry
        .resolve_import(&import)
        .expect("legacy slot must resolve");
}
