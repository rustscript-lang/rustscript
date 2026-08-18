use std::sync::Arc;

use vm::compiler::{CompileSourceFileOptions, SourceFlavor, TypeSchema};
use vm::{
    CallOutcome, CallReturn, HostApiBuilder, HostApiCatalog, HostApiFingerprint, HostFunction,
    HostFunctionRegistry, HostFunctionSchema, HostImport, HostImportParam, HostImportSchema,
    HostParamPassing, HostParamSchema, HostTypeSchema, Value, ValueType, Vm, VmResult, VmStatus,
    compile_source_with_flavor_and_options,
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

    let mut vm = Vm::new(compiled.program);
    registry
        .bind_vm_cached(&mut vm)
        .expect("exact bind should succeed");

    assert_eq!(vm.run().expect("run"), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(100), Value::Int(200)]);
}

fn int_exact_schema(fingerprint: u64) -> HostImportSchema {
    HostImportSchema {
        params: vec![HostImportParam {
            name: "value".into(),
            schema: TypeSchema::Int,
            passing: HostParamPassing::Value,
        }],
        return_type: TypeSchema::Int,
        fingerprint: HostApiFingerprint::from_raw(fingerprint),
    }
}

/// (2) Fingerprint differs (same param/return schema, different catalog fingerprint)
/// → `resolve_import` with an unmatched schema rejects and never falls back to the
/// legacy by-name slot.
#[test]
fn fingerprint_mismatch_rejected_without_by_name_fallback() {
    let schema_a = int_exact_schema(0xAAAA_0000_0000_0001);
    let schema_b = HostImportSchema {
        fingerprint: HostApiFingerprint::from_raw(0xBBBB_0000_0000_0001),
        ..schema_a.clone()
    };

    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact("echo::emit", 1, schema_a, tag_factory(5))
        .unwrap();

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
        error.to_string().contains("exact"),
        "error should state no exact match: {error}"
    );
}

/// (3) Param passing mismatch (exact `Value` vs import `BorrowMut`) → rejected.
#[test]
fn param_passing_schema_mismatch_rejected() {
    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact(
            "io::read",
            1,
            int_exact_schema(0xCCCC_0000_0000_0001),
            tag_factory(7),
        )
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
            fingerprint: HostApiFingerprint::from_raw(0xCCCC_0000_0000_0001),
        }),
    };
    let error = registry
        .resolve_import(&import)
        .expect_err("param passing mismatch must fail");
    assert!(
        error.to_string().contains("exact"),
        "error should state the exact-match failure: {error}"
    );
}

/// (4) Return-schema mismatch (exact Int vs import Bool return) → rejected.
#[test]
fn exact_return_schema_mismatch_rejected() {
    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact(
            "io::read",
            1,
            int_exact_schema(0xDDDD_0000_0000_0001),
            tag_factory(8),
        )
        .unwrap();

    let import = HostImport {
        name: "io::read".into(),
        arity: 1,
        return_type: ValueType::Bool,
        schema: Some(int_exact_schema(0xDDDD_0000_0000_0001)),
    };
    let error = registry
        .resolve_import(&import)
        .expect_err("return schema mismatch must fail");
    assert!(error.to_string().contains("exact"), "{error}");
}

/// (5) A legacy `schema:None` name-only binding cannot hijack an existing exact slot:
/// the exact binding still resolves to its own slot and its own function.
#[test]
fn legacy_name_only_binding_cannot_hijack_exact_slot() {
    let mut registry = HostFunctionRegistry::new();
    let slot_exact = registry
        .register_exact("srv::ping", 1, int_exact_schema(0x4444), tag_factory(5))
        .unwrap();
    registry.register("srv::ping", 1, tag_factory(9));

    let import = HostImport {
        name: "srv::ping".into(),
        arity: 1,
        return_type: ValueType::Int,
        schema: Some(int_exact_schema(0x4444)),
    };
    let slot = registry
        .resolve_import(&import)
        .expect("exact binding must win");
    assert_eq!(slot, slot_exact, "exact slot must not be hijacked");
}

/// (6) Duplicate exact (name+schema) registration → explicit deterministic error.
#[test]
fn duplicate_exact_registration_rejected() {
    let mut registry = HostFunctionRegistry::new();
    let schema = int_exact_schema(0x1111);
    registry
        .register_exact("io::read", 1, schema.clone(), tag_factory(1))
        .unwrap();
    let err = registry
        .register_exact("io::read", 1, schema, tag_factory(2))
        .expect_err("duplicate exact registration must error");
    assert!(err.to_string().contains("duplicate"), "{err}");
}

/// (7) Plan cache partitions by exact schema: same name & arity, different exact schemas
/// produce separate cache entries / distinct import signatures.
#[test]
fn plan_cache_partitions_by_exact_schema() {
    let mut registry = HostFunctionRegistry::new();
    registry
        .register_exact("calc::m", 1, int_exact_schema(0x15), tag_factory(1))
        .unwrap();
    registry
        .register_exact("calc::m", 1, int_exact_schema(0x16), tag_factory(2))
        .unwrap();

    let imports_1 = [HostImport {
        name: "calc::m".into(),
        arity: 1,
        return_type: ValueType::Int,
        schema: Some(int_exact_schema(0x15)),
    }];
    let before = registry.plan_cache_len();
    let plan_1 = registry.prepare_plan(&imports_1).unwrap();
    assert_eq!(before + 1, registry.plan_cache_len());

    let imports_2 = [HostImport {
        name: "calc::m".into(),
        arity: 1,
        return_type: ValueType::Int,
        schema: Some(int_exact_schema(0x16)),
    }];
    let plan_2 = registry.prepare_plan(&imports_2).unwrap();
    assert_eq!(
        before + 2,
        registry.plan_cache_len(),
        "different exact schema needs its own cache entry"
    );

    assert_ne!(plan_1.import_signature(), plan_2.import_signature());
}

/// (8) `schema: None` legacy static import path keeps working independently.
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
