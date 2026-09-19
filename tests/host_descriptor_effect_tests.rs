#![cfg(feature = "runtime")]

//! Characterization and behavior tests for host function descriptors,
//! guest resource effects, deterministic module aggregation, and
//! transactional installation.

use vm::catalog_import_schemas;
use vm::host_api::{
    HostApiBuilder, HostApiCatalog, HostFunctionSchema, HostParamPassing, HostParamSchema,
    HostStructField, HostStructSchema, HostTypeSchema, ResourceTypeKey, ResourceTypeSchema,
};

fn counter_key() -> ResourceTypeKey {
    ResourceTypeKey::new("demo.counter").expect("static key")
}

fn value_only_catalog() -> HostApiCatalog {
    let mut builder = HostApiBuilder::new();
    builder.function(HostFunctionSchema::with_return(
        "demo::add",
        vec![
            HostParamSchema::value("lhs", HostTypeSchema::Int),
            HostParamSchema::value("rhs", HostTypeSchema::Int),
        ],
        HostTypeSchema::Int,
    ));
    builder.build().expect("value-only catalog")
}

fn named_struct_catalog() -> HostApiCatalog {
    let point = HostStructSchema::new(
        "Point",
        vec![
            HostStructField::new("x", HostTypeSchema::Int),
            HostStructField::new("y", HostTypeSchema::Int),
        ],
    );
    let mut builder = HostApiBuilder::new();
    builder.named_struct(point.clone());
    builder.function(HostFunctionSchema::with_return(
        "geo::origin",
        vec![],
        point.as_type(),
    ));
    builder.build().expect("named-struct catalog")
}

fn borrowed_resource_catalog() -> HostApiCatalog {
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(counter_key(), "counter"));
    builder.function(HostFunctionSchema::with_return(
        "demo::read_counter",
        vec![HostParamSchema::with_passing(
            "counter",
            HostTypeSchema::Resource(counter_key()),
            HostParamPassing::Borrow,
        )],
        HostTypeSchema::Int,
    ));
    builder.build().expect("borrowed-resource catalog")
}

fn mutable_resource_catalog() -> HostApiCatalog {
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(counter_key(), "counter"));
    builder.function(HostFunctionSchema::with_return(
        "demo::bump_counter",
        vec![HostParamSchema::with_passing(
            "counter",
            HostTypeSchema::Resource(counter_key()),
            HostParamPassing::BorrowMut,
        )],
        HostTypeSchema::Int,
    ));
    builder.build().expect("mutable-resource catalog")
}

fn owned_resource_catalog() -> HostApiCatalog {
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(counter_key(), "counter"));
    builder.function(HostFunctionSchema::with_return(
        "demo::take_counter",
        vec![HostParamSchema::with_passing(
            "counter",
            HostTypeSchema::Resource(counter_key()),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::Int,
    ));
    builder.build().expect("owned-resource catalog")
}

fn resource_return_catalog() -> HostApiCatalog {
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(counter_key(), "counter"));
    builder.function(HostFunctionSchema::with_return(
        "demo::make_counter",
        vec![HostParamSchema::value("seed", HostTypeSchema::Int)],
        HostTypeSchema::Resource(counter_key()),
    ));
    builder.build().expect("resource-return catalog")
}

#[test]
fn value_only_catalog_fingerprint_is_stable() {
    let catalog = value_only_catalog();
    let schemas = catalog_import_schemas(&catalog, "demo::add");
    assert_eq!(schemas.len(), 1);
    assert_eq!(schemas[0].params.len(), 2);
    assert_eq!(schemas[0].params[0].passing, HostParamPassing::Value);
    assert_eq!(schemas[0].return_type, HostTypeSchema::Int);
    assert_eq!(schemas[0].fingerprint, catalog.fingerprint());
    let rebuilt = value_only_catalog();
    assert_eq!(catalog.fingerprint(), rebuilt.fingerprint());
    assert_eq!(catalog.fingerprint().to_string(), "900fe90d1c222a2a");
}

#[test]
fn named_struct_catalog_preserves_inline_fields() {
    let catalog = named_struct_catalog();
    let schemas = catalog_import_schemas(&catalog, "geo::origin");
    assert_eq!(schemas.len(), 1);
    assert_eq!(
        schemas[0].return_type,
        HostTypeSchema::named_struct(
            "Point",
            vec![
                HostStructField::new("x", HostTypeSchema::Int),
                HostStructField::new("y", HostTypeSchema::Int),
            ],
        )
    );
    let rebuilt = named_struct_catalog();
    assert_eq!(catalog.fingerprint(), rebuilt.fingerprint());
    assert_eq!(catalog.fingerprint().to_string(), "8e9381404a23e061");
}

#[test]
fn borrowed_resource_catalog_uses_borrow_passing() {
    let catalog = borrowed_resource_catalog();
    let schemas = catalog_import_schemas(&catalog, "demo::read_counter");
    assert_eq!(schemas[0].params[0].passing, HostParamPassing::Borrow);
    assert_eq!(
        schemas[0].params[0].schema,
        HostTypeSchema::Resource(counter_key())
    );
    let rebuilt = borrowed_resource_catalog();
    assert_eq!(catalog.fingerprint(), rebuilt.fingerprint());
    assert_eq!(catalog.fingerprint().to_string(), "a9669bd807fe6fee");
}

#[test]
fn mutable_resource_catalog_uses_borrow_mut_passing() {
    let catalog = mutable_resource_catalog();
    let schemas = catalog_import_schemas(&catalog, "demo::bump_counter");
    assert_eq!(schemas[0].params[0].passing, HostParamPassing::BorrowMut);
    let rebuilt = mutable_resource_catalog();
    assert_eq!(catalog.fingerprint(), rebuilt.fingerprint());
    assert_eq!(catalog.fingerprint().to_string(), "f7b1760ec6e726e9");
}

#[test]
fn owned_resource_catalog_uses_take_owned_passing() {
    let catalog = owned_resource_catalog();
    let schemas = catalog_import_schemas(&catalog, "demo::take_counter");
    assert_eq!(schemas[0].params[0].passing, HostParamPassing::TakeOwned);
    let rebuilt = owned_resource_catalog();
    assert_eq!(catalog.fingerprint(), rebuilt.fingerprint());
    assert_eq!(catalog.fingerprint().to_string(), "d523381d8a16c5a4");
}

#[test]
fn resource_return_catalog_declares_resource_schema() {
    let catalog = resource_return_catalog();
    let schemas = catalog_import_schemas(&catalog, "demo::make_counter");
    assert_eq!(
        schemas[0].return_type,
        HostTypeSchema::Resource(counter_key())
    );
    let rebuilt = resource_return_catalog();
    assert_eq!(catalog.fingerprint(), rebuilt.fingerprint());
    assert_eq!(catalog.fingerprint().to_string(), "7dbb2227244cabb2");
}

#[test]
fn documentation_does_not_change_fingerprints() {
    let semantic = borrowed_resource_catalog();
    let mut builder = HostApiBuilder::new();
    builder.resource(ResourceTypeSchema::new(
        counter_key(),
        "different documentation",
    ));
    builder.function(
        HostFunctionSchema::with_return(
            "demo::read_counter",
            vec![HostParamSchema::with_passing(
                "counter",
                HostTypeSchema::Resource(counter_key()),
                HostParamPassing::Borrow,
            )],
            HostTypeSchema::Int,
        )
        .with_description("different function docs"),
    );
    let documented = builder.build().expect("documented catalog");
    assert_eq!(semantic.fingerprint(), documented.fingerprint());
}

#[test]
fn guest_resource_effects_are_tied_to_existing_passing_modes() {
    use vm::host_api::{HostEffect, ResourceEffect};

    let borrow = ResourceEffect::borrow(counter_key());
    assert_eq!(borrow.passing(), Some(HostParamPassing::Borrow));
    assert_eq!(borrow.key(), &counter_key());

    let borrow_mut = ResourceEffect::borrow_mut(counter_key());
    assert_eq!(borrow_mut.passing(), Some(HostParamPassing::BorrowMut));

    let take = ResourceEffect::take_owned(counter_key());
    assert_eq!(take.passing(), Some(HostParamPassing::TakeOwned));

    let create = ResourceEffect::create(counter_key());
    assert_eq!(
        create.passing(),
        None,
        "creation is a return effect and must not invent a parameter passing mode"
    );

    let effects = [
        HostEffect::GuestResource(borrow),
        HostEffect::GuestResource(borrow_mut),
        HostEffect::GuestResource(take),
        HostEffect::GuestResource(create),
    ];
    assert!(
        effects
            .iter()
            .all(|effect| effect.guest_resource().is_some())
    );

    let catalog = borrowed_resource_catalog();
    assert_eq!(catalog.fingerprint().to_string(), "a9669bd807fe6fee");
}

#[test]
fn host_resource_type_exposes_canonical_key_and_description() {
    use vm::host_extension::HostResourceType;
    use vm::resource::{CloseProgress, HostResource, ResourceCloseReason};

    struct Counter;

    impl HostResource for Counter {
        fn begin_close(
            &mut self,
            _reason: ResourceCloseReason,
        ) -> vm::resource::ResourceResult<CloseProgress> {
            Ok(CloseProgress::Ready)
        }
    }

    impl HostResourceType for Counter {
        const KEY: &'static str = "demo.counter";
        const DESCRIPTION: &'static str = "An external counter resource";
    }

    assert_eq!(Counter::KEY, "demo.counter");
    assert_eq!(Counter::DESCRIPTION, "An external counter resource");
    let schema = Counter::resource_schema();
    assert_eq!(schema.key, counter_key());
    assert_eq!(schema.description, "An external counter resource");
    ResourceTypeKey::new(Counter::KEY).expect("HostResourceType::KEY must be a valid resource key");
}

fn noop_host(_vm: &mut vm::Vm, _args: &[vm::Value]) -> vm::VmResult<vm::CallOutcome> {
    Ok(vm::CallOutcome::Return(vm::CallReturn::None))
}

fn stack_descriptor(
    schema: HostFunctionSchema,
    effects: Vec<vm::host_api::HostEffect>,
) -> vm::host_extension::HostFunctionDescriptor {
    use vm::host_extension::{
        HostAdapterDescriptor, HostBindingDescriptor, HostBindingKind, HostFunctionDescriptor,
    };
    HostFunctionDescriptor {
        schema,
        binding: HostBindingDescriptor {
            kind: HostBindingKind::StaticStack,
        },
        effects,
        adapter: HostAdapterDescriptor::StaticStack(noop_host),
        resource_types: vec![],
    }
}

fn read_counter_descriptor() -> vm::host_extension::HostFunctionDescriptor {
    use vm::host_api::{HostEffect, ResourceEffect};
    stack_descriptor(
        HostFunctionSchema::with_return(
            "demo::read_counter",
            vec![HostParamSchema::with_passing(
                "counter",
                HostTypeSchema::Resource(counter_key()),
                HostParamPassing::Borrow,
            )],
            HostTypeSchema::Int,
        ),
        vec![HostEffect::GuestResource(ResourceEffect::borrow(
            counter_key(),
        ))],
    )
}

fn add_descriptor() -> vm::host_extension::HostFunctionDescriptor {
    stack_descriptor(
        HostFunctionSchema::with_return(
            "demo::add",
            vec![
                HostParamSchema::value("lhs", HostTypeSchema::Int),
                HostParamSchema::value("rhs", HostTypeSchema::Int),
            ],
            HostTypeSchema::Int,
        ),
        vec![],
    )
}

#[test]
fn host_function_descriptor_reproduces_borrowed_resource_guest_schema() {
    use vm::host_api::{HostEffect, ResourceEffect};
    use vm::host_extension::{
        HostAdapterDescriptor, HostBindingDescriptor, HostBindingKind, HostFunctionDescriptor,
    };

    let descriptor = HostFunctionDescriptor {
        schema: HostFunctionSchema::with_return(
            "demo::read_counter",
            vec![HostParamSchema::with_passing(
                "counter",
                HostTypeSchema::Resource(counter_key()),
                HostParamPassing::Borrow,
            )],
            HostTypeSchema::Int,
        ),
        binding: HostBindingDescriptor {
            kind: HostBindingKind::StaticStack,
        },
        effects: vec![HostEffect::GuestResource(ResourceEffect::borrow(
            counter_key(),
        ))],
        adapter: HostAdapterDescriptor::StaticStack(noop_host),
        resource_types: vec![],
    };

    assert_eq!(descriptor.schema.name, "demo::read_counter");
    assert_eq!(
        descriptor.effects[0]
            .guest_resource()
            .expect("guest resource effect")
            .passing(),
        Some(HostParamPassing::Borrow)
    );
    assert_eq!(descriptor.binding.kind, HostBindingKind::StaticStack);

    let catalog = HostFunctionDescriptor::collect_catalog(std::slice::from_ref(&descriptor))
        .expect("catalog from descriptor");
    assert_eq!(
        catalog.fingerprint(),
        borrowed_resource_catalog().fingerprint()
    );
    let schemas = catalog_import_schemas(&catalog, "demo::read_counter");
    assert_eq!(schemas[0].params[0].passing, HostParamPassing::Borrow);
    assert_eq!(
        schemas[0].params[0].schema,
        HostTypeSchema::Resource(counter_key())
    );
}

#[test]
fn host_function_descriptor_reproduces_named_struct_guest_schema() {
    use vm::host_extension::{
        HostAdapterDescriptor, HostBindingDescriptor, HostBindingKind, HostFunctionDescriptor,
    };

    let point = HostStructSchema::new(
        "Point",
        vec![
            HostStructField::new("x", HostTypeSchema::Int),
            HostStructField::new("y", HostTypeSchema::Int),
        ],
    );
    let descriptor = HostFunctionDescriptor {
        schema: HostFunctionSchema::with_return("geo::origin", vec![], point.as_type()),
        binding: HostBindingDescriptor {
            kind: HostBindingKind::StaticStack,
        },
        effects: vec![],
        adapter: HostAdapterDescriptor::StaticStack(noop_host),
        resource_types: vec![],
    };

    let catalog = HostFunctionDescriptor::collect_catalog(std::slice::from_ref(&descriptor))
        .expect("catalog from named-struct descriptor");
    assert_eq!(catalog.fingerprint(), named_struct_catalog().fingerprint());
    let schemas = catalog_import_schemas(&catalog, "geo::origin");
    assert_eq!(
        schemas[0].return_type,
        HostTypeSchema::named_struct(
            "Point",
            vec![
                HostStructField::new("x", HostTypeSchema::Int),
                HostStructField::new("y", HostTypeSchema::Int),
            ],
        )
    );
}

#[test]
fn host_module_descriptor_aggregates_explicit_function_list() {
    use vm::host_extension::HostModuleDescriptor;

    let module = HostModuleDescriptor {
        name: "demo",
        functions: &[read_counter_descriptor, add_descriptor],
        resources: &[],
    };
    let catalog = module.catalog().expect("module catalog");
    let mut expected = HostApiBuilder::new();
    expected.resource(ResourceTypeSchema::new(counter_key(), "counter"));
    expected.function(HostFunctionSchema::with_return(
        "demo::read_counter",
        vec![HostParamSchema::with_passing(
            "counter",
            HostTypeSchema::Resource(counter_key()),
            HostParamPassing::Borrow,
        )],
        HostTypeSchema::Int,
    ));
    expected.function(HostFunctionSchema::with_return(
        "demo::add",
        vec![
            HostParamSchema::value("lhs", HostTypeSchema::Int),
            HostParamSchema::value("rhs", HostTypeSchema::Int),
        ],
        HostTypeSchema::Int,
    ));
    let expected = expected.build().expect("expected catalog");
    assert_eq!(catalog.fingerprint(), expected.fingerprint());
    assert_eq!(
        catalog_import_schemas(&catalog, "demo::read_counter")[0].params[0].passing,
        HostParamPassing::Borrow
    );
    assert_eq!(
        catalog_import_schemas(&catalog, "demo::add")[0]
            .params
            .len(),
        2
    );
}

#[test]
fn host_module_install_registers_catalog_adapters() {
    use vm::HostFunctionRegistry;
    use vm::host_extension::HostModuleDescriptor;

    let module = HostModuleDescriptor {
        name: "demo",
        functions: &[read_counter_descriptor],
        resources: &[],
    };
    let mut registry = HostFunctionRegistry::empty();
    let catalog = module
        .install(&mut registry)
        .expect("descriptor module should install");
    assert!(registry.contains_name("demo::read_counter"));
    assert_eq!(
        catalog.fingerprint(),
        borrowed_resource_catalog().fingerprint()
    );
}

#[test]
fn host_module_install_rolls_back_on_later_conflict() {
    use vm::HostFunctionRegistry;
    use vm::host_extension::HostModuleDescriptor;

    let mut registry = HostFunctionRegistry::empty();
    let existing = borrowed_resource_catalog();
    let schema = catalog_import_schemas(&existing, "demo::read_counter")[0].clone();
    registry
        .register_catalog_static_stack(schema, noop_host)
        .expect("seed existing function");
    assert!(registry.contains_name("demo::read_counter"));

    let plan_before = registry
        .prepare_shared_plan(&[])
        .expect("baseline plan should build");

    let module = HostModuleDescriptor {
        name: "demo",
        functions: &[add_descriptor, read_counter_descriptor],
        resources: &[],
    };
    let error = module
        .install(&mut registry)
        .expect_err("duplicate function must fail the whole module");
    assert!(
        error.to_string().contains("demo::read_counter"),
        "error should name the conflicting function, got {error}"
    );
    assert!(registry.contains_name("demo::read_counter"));
    assert!(
        !registry.contains_name("demo::add"),
        "a function that registered in the staged snapshot must be absent after rollback"
    );

    let plan_after = registry
        .prepare_shared_plan(&[])
        .expect("plan after failed install should still build");
    assert!(
        std::sync::Arc::ptr_eq(&plan_before, &plan_after),
        "failed install must leave registry generation and cache identity unchanged"
    );
}

#[test]
fn conflicting_host_resource_types_fail_before_registry_mutation() {
    use std::any::TypeId;
    use vm::HostFunctionRegistry;
    use vm::host_extension::{HostModuleDescriptor, HostResourceType, HostResourceTypeMeta};
    use vm::resource::{CloseProgress, HostResource, ResourceCloseReason};

    struct CounterA;
    struct CounterB;

    impl HostResource for CounterA {
        fn begin_close(
            &mut self,
            _reason: ResourceCloseReason,
        ) -> vm::resource::ResourceResult<CloseProgress> {
            Ok(CloseProgress::Ready)
        }
    }
    impl HostResourceType for CounterA {
        const KEY: &'static str = "demo.counter";
        const DESCRIPTION: &'static str = "counter A";
    }

    impl HostResource for CounterB {
        fn begin_close(
            &mut self,
            _reason: ResourceCloseReason,
        ) -> vm::resource::ResourceResult<CloseProgress> {
            Ok(CloseProgress::Ready)
        }
    }
    impl HostResourceType for CounterB {
        const KEY: &'static str = "demo.counter";
        const DESCRIPTION: &'static str = "counter B";
    }

    let mut first = read_counter_descriptor();
    first.resource_types = vec![HostResourceTypeMeta::of::<CounterA>()];
    let mut second = add_descriptor();
    second.resource_types = vec![HostResourceTypeMeta::of::<CounterB>()];
    assert_ne!(TypeId::of::<CounterA>(), TypeId::of::<CounterB>());

    let mut registry = HostFunctionRegistry::empty();
    let error = HostModuleDescriptor::install_descriptors(&mut registry, &[first, second])
        .expect_err("conflicting concrete types for one key must fail");
    assert!(
        error.to_string().contains("demo.counter"),
        "conflict diagnostic should name the resource key, got {error}"
    );
    assert!(
        !registry.contains_name("demo::read_counter"),
        "conflict must be detected before registry mutation"
    );
    assert!(!registry.contains_name("demo::add"));
}

fn args_noop(_args: &[vm::Value]) -> vm::VmResult<vm::CallOutcome> {
    Ok(vm::CallOutcome::Return(vm::CallReturn::None))
}

fn yield_args(_args: &[vm::Value]) -> vm::VmResult<vm::CallOutcome> {
    Ok(vm::CallOutcome::Yield)
}

fn assert_registry_unmodified(
    registry: &vm::HostFunctionRegistry,
    plan_before: &std::sync::Arc<vm::HostBindingPlan>,
    unexpected: &[&str],
) {
    for name in unexpected {
        assert!(
            !registry.contains_name(name),
            "{name} must be absent after a failed install"
        );
    }
    let plan_after = registry
        .prepare_shared_plan(&[])
        .expect("plan after failed install should still build");
    assert!(
        std::sync::Arc::ptr_eq(plan_before, &plan_after),
        "failed install must leave registry generation and cache identity unchanged"
    );
}

#[test]
fn in_module_duplicate_function_identity_leaves_registry_unchanged() {
    use vm::HostFunctionRegistry;
    use vm::host_extension::HostModuleDescriptor;

    let mut registry = HostFunctionRegistry::empty();
    let plan_before = registry
        .prepare_shared_plan(&[])
        .expect("baseline plan should build");
    let error = HostModuleDescriptor::install_descriptors(
        &mut registry,
        &[add_descriptor(), add_descriptor()],
    )
    .expect_err("duplicate function identity in one module must fail");
    assert!(
        error.to_string().contains("demo::add"),
        "duplicate identity diagnostic should name the function, got {error}"
    );
    assert_registry_unmodified(&registry, &plan_before, &["demo::add"]);
}

#[test]
fn restricted_registry_installs_descriptor_module() {
    use vm::HostFunctionRegistry;
    use vm::host_extension::HostModuleDescriptor;

    let module = HostModuleDescriptor {
        name: "demo",
        functions: &[add_descriptor],
        resources: &[],
    };
    let mut registry = HostFunctionRegistry::restricted();
    let catalog = module
        .install(&mut registry)
        .expect("restricted registry should still accept descriptor install");
    assert!(registry.contains_name("demo::add"));
    assert_eq!(catalog.fingerprint(), value_only_catalog().fingerprint());
}

#[test]
fn adapter_binding_mismatch_rolls_back_successful_staged_adapter() {
    use vm::HostFunctionRegistry;
    use vm::host_extension::{
        HostAdapterDescriptor, HostBindingDescriptor, HostBindingKind, HostFunctionDescriptor,
        HostModuleDescriptor,
    };

    let mut registry = HostFunctionRegistry::empty();
    let plan_before = registry
        .prepare_shared_plan(&[])
        .expect("baseline plan should build");
    let error = HostModuleDescriptor::install_descriptors(
        &mut registry,
        &[
            add_descriptor(),
            HostFunctionDescriptor {
                schema: HostFunctionSchema::with_return(
                    "demo::mismatch",
                    vec![HostParamSchema::value("value", HostTypeSchema::Int)],
                    HostTypeSchema::Int,
                ),
                binding: HostBindingDescriptor {
                    kind: HostBindingKind::StaticStack,
                },
                effects: vec![],
                adapter: HostAdapterDescriptor::StaticArgs(args_noop),
                resource_types: vec![],
            },
        ],
    )
    .expect_err("binding/adapter mismatch must fail");
    assert!(
        error.to_string().contains("demo::mismatch"),
        "mismatch diagnostic should name the function, got {error}"
    );
    assert_registry_unmodified(&registry, &plan_before, &["demo::add", "demo::mismatch"]);
}

#[test]
fn named_struct_field_conflict_is_rejected_before_registry_mutation() {
    use vm::HostFunctionRegistry;
    use vm::host_extension::HostModuleDescriptor;

    let origin = stack_descriptor(
        HostFunctionSchema::with_return(
            "geo::origin",
            vec![],
            HostTypeSchema::named_struct(
                "Point",
                vec![
                    HostStructField::new("x", HostTypeSchema::Int),
                    HostStructField::new("y", HostTypeSchema::Int),
                ],
            ),
        ),
        vec![],
    );
    let shifted = stack_descriptor(
        HostFunctionSchema::with_return(
            "geo::shifted",
            vec![],
            HostTypeSchema::named_struct(
                "Point",
                vec![
                    HostStructField::new("x", HostTypeSchema::Int),
                    HostStructField::new("z", HostTypeSchema::Int),
                ],
            ),
        ),
        vec![],
    );
    let mut registry = HostFunctionRegistry::empty();
    let plan_before = registry
        .prepare_shared_plan(&[])
        .expect("baseline plan should build");
    let error = HostModuleDescriptor::install_descriptors(&mut registry, &[origin, shifted])
        .expect_err("conflicting named-struct fields must fail");
    assert!(
        error.to_string().contains("Point"),
        "field-conflict diagnostic should name the struct, got {error}"
    );
    assert_registry_unmodified(&registry, &plan_before, &["geo::origin", "geo::shifted"]);
}

#[test]
fn duplicate_compatible_resource_declarations_succeed() {
    use vm::host_extension::{HostFunctionDescriptor, HostResourceTypeMeta};

    fn counter_meta() -> HostResourceTypeMeta {
        use vm::host_extension::HostResourceType;
        use vm::resource::{CloseProgress, HostResource, ResourceCloseReason};
        struct Counter;
        impl HostResource for Counter {
            fn begin_close(
                &mut self,
                _reason: ResourceCloseReason,
            ) -> vm::resource::ResourceResult<CloseProgress> {
                Ok(CloseProgress::Ready)
            }
        }
        impl HostResourceType for Counter {
            const KEY: &'static str = "demo.counter";
            const DESCRIPTION: &'static str = "An external counter resource";
        }
        HostResourceTypeMeta::of::<Counter>()
    }

    let mut first = read_counter_descriptor();
    first.resource_types = vec![counter_meta()];
    let mut second = stack_descriptor(
        HostFunctionSchema::with_return(
            "demo::peek_counter",
            vec![HostParamSchema::with_passing(
                "counter",
                HostTypeSchema::Resource(counter_key()),
                HostParamPassing::Borrow,
            )],
            HostTypeSchema::Int,
        ),
        vec![vm::host_api::HostEffect::GuestResource(
            vm::host_api::ResourceEffect::borrow(counter_key()),
        )],
    );
    second.resource_types = vec![counter_meta()];
    let catalog = HostFunctionDescriptor::collect_catalog(&[first, second])
        .expect("identical resource declarations must dedupe");
    assert!(catalog.has_resource(&counter_key()));
    assert_eq!(
        catalog
            .resources()
            .iter()
            .filter(|resource| resource.key == counter_key())
            .count(),
        1
    );
    assert_eq!(
        catalog
            .resources()
            .iter()
            .find(|resource| resource.key == counter_key())
            .map(|resource| resource.description.as_str()),
        Some("An external counter resource")
    );
}

#[test]
fn resource_description_conflict_is_rejected() {
    use vm::host_extension::{HostFunctionDescriptor, HostResourceType, HostResourceTypeMeta};
    use vm::resource::{CloseProgress, HostResource, ResourceCloseReason};

    struct Counter;
    impl HostResource for Counter {
        fn begin_close(
            &mut self,
            _reason: ResourceCloseReason,
        ) -> vm::resource::ResourceResult<CloseProgress> {
            Ok(CloseProgress::Ready)
        }
    }
    impl HostResourceType for Counter {
        const KEY: &'static str = "demo.counter";
        const DESCRIPTION: &'static str = "counter A";
    }

    let mut first = read_counter_descriptor();
    first.resource_types = vec![HostResourceTypeMeta::of::<Counter>()];
    let mut second = add_descriptor();
    second.resource_types = vec![HostResourceTypeMeta {
        schema: ResourceTypeSchema::new(counter_key(), "counter B"),
        type_id: std::any::TypeId::of::<Counter>(),
        type_name: std::any::type_name::<Counter>(),
    }];
    let error = HostFunctionDescriptor::collect_catalog(&[first, second])
        .expect_err("same type with a different description must fail");
    assert!(
        error.to_string().contains("demo.counter"),
        "description conflict should name the key, got {error}"
    );
}

fn expected_effects_from_schema(schema: &HostFunctionSchema) -> Vec<vm::host_api::ResourceEffect> {
    use vm::host_api::ResourceEffect;
    let mut expected = Vec::new();
    for param in &schema.params {
        if let HostTypeSchema::Resource(key) = &param.ty {
            expected.push(match param.passing {
                HostParamPassing::Borrow => ResourceEffect::borrow(key.clone()),
                HostParamPassing::BorrowMut => ResourceEffect::borrow_mut(key.clone()),
                HostParamPassing::TakeOwned => ResourceEffect::take_owned(key.clone()),
                HostParamPassing::Value => continue,
            });
        }
    }
    if let HostTypeSchema::Resource(key) = &schema.return_type {
        expected.push(ResourceEffect::create(key.clone()));
    }
    expected
}

#[test]
fn collect_catalog_rejects_missing_guest_resource_effect() {
    use vm::host_extension::HostFunctionDescriptor;

    let descriptor = stack_descriptor(
        HostFunctionSchema::with_return(
            "demo::read_counter",
            vec![HostParamSchema::with_passing(
                "counter",
                HostTypeSchema::Resource(counter_key()),
                HostParamPassing::Borrow,
            )],
            HostTypeSchema::Int,
        ),
        vec![],
    );
    let error = HostFunctionDescriptor::collect_catalog(std::slice::from_ref(&descriptor))
        .expect_err("missing guest-resource effect must fail");
    assert!(
        error.to_string().contains("demo::read_counter"),
        "missing-effect diagnostic should name the function, got {error}"
    );
}

#[test]
fn collect_catalog_rejects_extra_duplicate_wrong_key_and_wrong_mode_effects() {
    use vm::host_api::{HostEffect, ResourceEffect};
    use vm::host_extension::HostFunctionDescriptor;

    let schema = HostFunctionSchema::with_return(
        "demo::read_counter",
        vec![HostParamSchema::with_passing(
            "counter",
            HostTypeSchema::Resource(counter_key()),
            HostParamPassing::Borrow,
        )],
        HostTypeSchema::Int,
    );
    let extra = stack_descriptor(
        schema.clone(),
        vec![
            HostEffect::GuestResource(ResourceEffect::borrow(counter_key())),
            HostEffect::GuestResource(ResourceEffect::borrow(
                ResourceTypeKey::new("demo.widget").expect("key"),
            )),
        ],
    );
    let extra_error = HostFunctionDescriptor::collect_catalog(std::slice::from_ref(&extra))
        .expect_err("extra guest-resource effect must fail");
    assert!(extra_error.to_string().contains("demo::read_counter"));

    let duplicate = stack_descriptor(
        schema.clone(),
        vec![
            HostEffect::GuestResource(ResourceEffect::borrow(counter_key())),
            HostEffect::GuestResource(ResourceEffect::borrow(counter_key())),
        ],
    );
    let duplicate_error = HostFunctionDescriptor::collect_catalog(std::slice::from_ref(&duplicate))
        .expect_err("duplicate guest-resource effect must fail");
    assert!(duplicate_error.to_string().contains("demo::read_counter"));

    let wrong_key = stack_descriptor(
        schema.clone(),
        vec![HostEffect::GuestResource(ResourceEffect::borrow(
            ResourceTypeKey::new("demo.widget").expect("key"),
        ))],
    );
    let wrong_key_error = HostFunctionDescriptor::collect_catalog(std::slice::from_ref(&wrong_key))
        .expect_err("wrong-key guest-resource effect must fail");
    assert!(wrong_key_error.to_string().contains("demo::read_counter"));

    let wrong_mode = stack_descriptor(
        schema,
        vec![HostEffect::GuestResource(ResourceEffect::borrow_mut(
            counter_key(),
        ))],
    );
    let wrong_mode_error =
        HostFunctionDescriptor::collect_catalog(std::slice::from_ref(&wrong_mode))
            .expect_err("wrong-mode guest-resource effect must fail");
    assert!(wrong_mode_error.to_string().contains("demo::read_counter"));
}

#[test]
fn collect_catalog_rejects_create_and_return_mismatches() {
    use vm::host_api::{HostEffect, ResourceEffect};
    use vm::host_extension::HostFunctionDescriptor;

    let create_on_param = stack_descriptor(
        HostFunctionSchema::with_return(
            "demo::read_counter",
            vec![HostParamSchema::with_passing(
                "counter",
                HostTypeSchema::Resource(counter_key()),
                HostParamPassing::Borrow,
            )],
            HostTypeSchema::Int,
        ),
        vec![HostEffect::GuestResource(ResourceEffect::create(
            counter_key(),
        ))],
    );
    HostFunctionDescriptor::collect_catalog(std::slice::from_ref(&create_on_param))
        .expect_err("create effect on a parameter must fail");

    let missing_create = stack_descriptor(
        HostFunctionSchema::with_return(
            "demo::make_counter",
            vec![HostParamSchema::value("seed", HostTypeSchema::Int)],
            HostTypeSchema::Resource(counter_key()),
        ),
        vec![],
    );
    HostFunctionDescriptor::collect_catalog(std::slice::from_ref(&missing_create))
        .expect_err("resource return without create effect must fail");

    let wrong_create_key = stack_descriptor(
        HostFunctionSchema::with_return(
            "demo::make_counter",
            vec![HostParamSchema::value("seed", HostTypeSchema::Int)],
            HostTypeSchema::Resource(counter_key()),
        ),
        vec![HostEffect::GuestResource(ResourceEffect::create(
            ResourceTypeKey::new("demo.widget").expect("key"),
        ))],
    );
    HostFunctionDescriptor::collect_catalog(std::slice::from_ref(&wrong_create_key))
        .expect_err("create effect with the wrong key must fail");
}

#[test]
fn inferred_borrow_mut_take_and_create_effects_match_schema() {
    use vm::host_api::{HostEffect, ResourceEffect};
    use vm::host_extension::HostFunctionDescriptor;

    let borrow = read_counter_descriptor();
    assert_eq!(
        expected_effects_from_schema(&borrow.schema),
        vec![ResourceEffect::borrow(counter_key())]
    );
    HostFunctionDescriptor::collect_catalog(std::slice::from_ref(&borrow))
        .expect("inferred borrow effect should match schema");

    let borrow_mut = stack_descriptor(
        HostFunctionSchema::with_return(
            "demo::bump_counter",
            vec![HostParamSchema::with_passing(
                "counter",
                HostTypeSchema::Resource(counter_key()),
                HostParamPassing::BorrowMut,
            )],
            HostTypeSchema::Int,
        ),
        vec![HostEffect::GuestResource(ResourceEffect::borrow_mut(
            counter_key(),
        ))],
    );
    HostFunctionDescriptor::collect_catalog(std::slice::from_ref(&borrow_mut))
        .expect("inferred borrow_mut effect should match schema");

    let take = stack_descriptor(
        HostFunctionSchema::with_return(
            "demo::take_counter",
            vec![HostParamSchema::with_passing(
                "counter",
                HostTypeSchema::Resource(counter_key()),
                HostParamPassing::TakeOwned,
            )],
            HostTypeSchema::Int,
        ),
        vec![HostEffect::GuestResource(ResourceEffect::take_owned(
            counter_key(),
        ))],
    );
    HostFunctionDescriptor::collect_catalog(std::slice::from_ref(&take))
        .expect("inferred take effect should match schema");

    let create = stack_descriptor(
        HostFunctionSchema::with_return(
            "demo::make_counter",
            vec![HostParamSchema::value("seed", HostTypeSchema::Int)],
            HostTypeSchema::Resource(counter_key()),
        ),
        vec![HostEffect::GuestResource(ResourceEffect::create(
            counter_key(),
        ))],
    );
    HostFunctionDescriptor::collect_catalog(std::slice::from_ref(&create))
        .expect("inferred create effect should match schema");
}

#[test]
fn host_module_preserves_declaration_order_and_module_resource_metadata() {
    use vm::host_extension::{HostModuleDescriptor, HostResourceType, HostResourceTypeMeta};
    use vm::resource::{CloseProgress, HostResource, ResourceCloseReason};

    struct Counter;
    impl HostResource for Counter {
        fn begin_close(
            &mut self,
            _reason: ResourceCloseReason,
        ) -> vm::resource::ResourceResult<CloseProgress> {
            Ok(CloseProgress::Ready)
        }
    }
    impl HostResourceType for Counter {
        const KEY: &'static str = "demo.counter";
        const DESCRIPTION: &'static str = "module counter";
    }
    fn counter_resource() -> HostResourceTypeMeta {
        HostResourceTypeMeta::of::<Counter>()
    }

    let module = HostModuleDescriptor {
        name: "demo",
        functions: &[add_descriptor, read_counter_descriptor],
        resources: &[counter_resource],
    };
    let descriptors = module.descriptors();
    assert!(
        descriptors[0].resource_types.is_empty(),
        "module resources must not be stuffed onto the first function descriptor"
    );
    let catalog = module.catalog().expect("module catalog");
    assert_eq!(
        catalog
            .functions()
            .iter()
            .map(|function| function.name.as_str())
            .collect::<Vec<_>>(),
        vec!["demo::add", "demo::read_counter"]
    );
    let resource = catalog
        .resources()
        .iter()
        .find(|resource| resource.key == counter_key())
        .expect("module resource");
    assert_eq!(resource.description, "module counter");
}

#[test]
fn empty_function_module_reports_a_clear_install_error() {
    use vm::HostFunctionRegistry;
    use vm::host_extension::{HostModuleDescriptor, HostResourceType, HostResourceTypeMeta};
    use vm::resource::{CloseProgress, HostResource, ResourceCloseReason};

    struct Counter;
    impl HostResource for Counter {
        fn begin_close(
            &mut self,
            _reason: ResourceCloseReason,
        ) -> vm::resource::ResourceResult<CloseProgress> {
            Ok(CloseProgress::Ready)
        }
    }
    impl HostResourceType for Counter {
        const KEY: &'static str = "demo.counter";
        const DESCRIPTION: &'static str = "module counter";
    }
    fn counter_resource() -> HostResourceTypeMeta {
        HostResourceTypeMeta::of::<Counter>()
    }

    let module = HostModuleDescriptor {
        name: "demo",
        functions: &[],
        resources: &[counter_resource],
    };
    let catalog = module
        .catalog()
        .expect("resource-only catalog should still build");
    assert!(catalog.has_resource(&counter_key()));
    let mut registry = HostFunctionRegistry::empty();
    let error = module
        .install(&mut registry)
        .expect_err("installing a module with no functions must fail closed");
    assert!(
        error.to_string().contains("demo") && error.to_string().contains("function"),
        "empty-function diagnostic should name the module, got {error}"
    );
}

#[test]
fn non_yielding_binding_rejects_yield_unlike_ordinary_static_args() {
    use vm::host_extension::{
        HostAdapterDescriptor, HostBindingDescriptor, HostBindingKind, HostFunctionDescriptor,
        HostModuleDescriptor,
    };
    use vm::{HostFunctionRegistry, HostImport, Program, Value, Vm, VmError, VmStatus};

    let schema = HostFunctionSchema::with_return("demo::tick", vec![], HostTypeSchema::Int);
    let non_yielding = HostFunctionDescriptor {
        schema: schema.clone(),
        binding: HostBindingDescriptor {
            kind: HostBindingKind::StaticNonYieldingArgs,
        },
        effects: vec![],
        adapter: HostAdapterDescriptor::StaticNonYieldingArgs(yield_args),
        resource_types: vec![],
    };
    let mut registry = HostFunctionRegistry::empty();
    HostModuleDescriptor::install_descriptors(&mut registry, std::slice::from_ref(&non_yielding))
        .expect("non-yielding descriptor should install");

    let import = catalog_import_schemas(
        &HostFunctionDescriptor::collect_catalog(std::slice::from_ref(&non_yielding))
            .expect("catalog"),
        "demo::tick",
    )
    .into_iter()
    .next()
    .expect("schema");
    let mut bytecode = vm::BytecodeBuilder::new();
    bytecode.call(0, 0);
    bytecode.ret();
    let program = Program::with_imports_and_debug(
        Vec::new(),
        bytecode.finish(),
        vec![HostImport {
            name: "demo::tick".to_string(),
            arity: 0,
            return_type: vm::ValueType::Int,
        }],
        None,
    )
    .with_host_import_schemas(vec![import])
    .expect("schema metadata");
    let mut vm = Vm::new(program);
    registry.bind_vm_cached(&mut vm).expect("bind");
    let error = match vm.run() {
        Err(error) => error,
        Ok(status) => panic!("non-yielding yield must be a host error, got {status:?}"),
    };
    assert!(
        error.to_string().to_ascii_lowercase().contains("yield")
            || matches!(error, VmError::HostError(_)),
        "non-yielding adapter must reject Yield, got {error}"
    );

    let ordinary = HostFunctionDescriptor {
        schema,
        binding: HostBindingDescriptor {
            kind: HostBindingKind::StaticArgs,
        },
        effects: vec![],
        adapter: HostAdapterDescriptor::StaticArgs(yield_args),
        resource_types: vec![],
    };
    let mut args_registry = HostFunctionRegistry::empty();
    HostModuleDescriptor::install_descriptors(&mut args_registry, std::slice::from_ref(&ordinary))
        .expect("ordinary StaticArgs descriptor should install");
    let import = catalog_import_schemas(
        &HostFunctionDescriptor::collect_catalog(std::slice::from_ref(&ordinary)).expect("catalog"),
        "demo::tick",
    )
    .into_iter()
    .next()
    .expect("schema");
    let mut bytecode = vm::BytecodeBuilder::new();
    bytecode.call(0, 0);
    bytecode.ret();
    let program = Program::with_imports_and_debug(
        Vec::new(),
        bytecode.finish(),
        vec![HostImport {
            name: "demo::tick".to_string(),
            arity: 0,
            return_type: vm::ValueType::Int,
        }],
        None,
    )
    .with_host_import_schemas(vec![import])
    .expect("schema metadata");
    let mut vm = Vm::new(program);
    args_registry.bind_vm_cached(&mut vm).expect("bind");
    assert_eq!(
        vm.run().expect("ordinary StaticArgs may yield"),
        VmStatus::Yielded
    );
    let _ = Value::Null;
}

#[test]
fn invalid_resource_type_key_is_fallible() {
    use vm::host_extension::{HostResourceType, HostResourceTypeMeta};
    use vm::resource::{CloseProgress, HostResource, ResourceCloseReason};

    struct Bad;
    impl HostResource for Bad {
        fn begin_close(
            &mut self,
            _reason: ResourceCloseReason,
        ) -> vm::resource::ResourceResult<CloseProgress> {
            Ok(CloseProgress::Ready)
        }
    }
    impl HostResourceType for Bad {
        const KEY: &'static str = "!!!";
        const DESCRIPTION: &'static str = "bad";
    }
    let error = HostResourceTypeMeta::try_of::<Bad>().expect_err("invalid key must fail closed");
    assert!(
        error.to_string().contains("!!!") || error.to_string().contains("invalid"),
        "fallible metadata should name the invalid key, got {error}"
    );
}

// ── Host-private state effects ──────────────────────────────────────────────

/// A per-VM state type used by the descriptor state-effect tests.
#[derive(Debug, Default, PartialEq, Eq)]
struct TuningCache {
    value: i64,
}

impl vm::host_api::HostState for TuningCache {
    const KEY: &'static str = "demo.tuning_cache";

    fn initialize() -> Result<Self, String> {
        Ok(Self::default())
    }
}

/// A second state type that claims no key shared with `TuningCache`.
#[derive(Debug, Default, PartialEq, Eq)]
struct AuditLog {
    lines: u64,
}

impl vm::host_api::HostState for AuditLog {
    const KEY: &'static str = "demo.audit_log";

    fn initialize() -> Result<Self, String> {
        Ok(Self::default())
    }
}

/// A state type that (incorrectly) claims `TuningCache`'s key.
#[derive(Debug, Default, PartialEq, Eq)]
struct ImpostorCache {
    value: i64,
}

impl vm::host_api::HostState for ImpostorCache {
    const KEY: &'static str = "demo.tuning_cache";

    fn initialize() -> Result<Self, String> {
        Ok(Self::default())
    }
}

fn state_effect_descriptor(
    name: &str,
    effects: Vec<vm::host_api::HostEffect>,
) -> vm::host_extension::HostFunctionDescriptor {
    stack_descriptor(
        HostFunctionSchema::with_return(name, vec![], HostTypeSchema::Int),
        effects,
    )
}

#[test]
fn hidden_state_effects_never_change_guest_schema_or_fingerprint() {
    use vm::host_api::{HostEffect, HostStateEffect};
    use vm::host_extension::HostFunctionDescriptor;

    let with_state = state_effect_descriptor(
        "demo::cached_tick",
        vec![HostEffect::HostState(
            HostStateEffect::write::<TuningCache>(),
        )],
    );
    let without_state = state_effect_descriptor("demo::cached_tick", Vec::new());

    assert_eq!(
        with_state.schema.params, without_state.schema.params,
        "host-private state must not add guest parameters"
    );
    assert_eq!(
        with_state.schema.return_type,
        without_state.schema.return_type
    );

    let state_catalog = HostFunctionDescriptor::collect_catalog(std::slice::from_ref(&with_state))
        .expect("state-declaring descriptors aggregate");
    let plain_catalog =
        HostFunctionDescriptor::collect_catalog(std::slice::from_ref(&without_state))
            .expect("descriptors without state aggregate");
    assert_eq!(
        state_catalog.fingerprint(),
        plain_catalog.fingerprint(),
        "host-private state must stay out of the catalog fingerprint"
    );

    let requirements = with_state.state_requirements();
    assert_eq!(requirements.len(), 1);
    assert_eq!(requirements[0].key(), "demo.tuning_cache");
    assert!(
        requirements[0].write,
        "a write effect is reported as a write"
    );
    assert_eq!(
        requirements[0].lifetime(),
        vm::host_api::HostStateLifetime::Vm
    );

    let read = HostStateEffect::read::<TuningCache>();
    assert!(read.is_same_state(&HostStateEffect::write::<TuningCache>()));
    assert!(!read.is_write());
    assert!(!read.is_same_state(&HostStateEffect::read::<AuditLog>()));
}

#[test]
fn module_state_requirements_deduplicate_and_keep_write_semantics() {
    use vm::host_api::{HostEffect, HostStateEffect};
    use vm::host_extension::HostModuleDescriptor;

    fn read_tuning() -> vm::host_extension::HostFunctionDescriptor {
        state_effect_descriptor(
            "demo::read_tuning",
            vec![HostEffect::HostState(HostStateEffect::read::<TuningCache>())],
        )
    }

    fn write_tuning() -> vm::host_extension::HostFunctionDescriptor {
        state_effect_descriptor(
            "demo::write_tuning",
            vec![HostEffect::HostState(
                HostStateEffect::write::<TuningCache>(),
            )],
        )
    }

    let module = HostModuleDescriptor {
        name: "demo.state",
        functions: &[read_tuning, write_tuning],
        resources: &[],
    };
    let requirements = module
        .state_requirements()
        .expect("identical providers deduplicate");
    assert_eq!(requirements.len(), 1, "one state, one requirement");
    assert!(requirements[0].write, "the write requirement wins");
}

#[test]
fn module_state_requirements_reject_conflicting_providers_before_registry_mutation() {
    use vm::HostFunctionRegistry;
    use vm::host_api::{HostEffect, HostStateEffect};
    use vm::host_extension::HostModuleDescriptor;

    fn read_tuning() -> vm::host_extension::HostFunctionDescriptor {
        state_effect_descriptor(
            "demo::read_tuning",
            vec![HostEffect::HostState(HostStateEffect::read::<TuningCache>())],
        )
    }

    fn read_impostor() -> vm::host_extension::HostFunctionDescriptor {
        state_effect_descriptor(
            "demo::read_impostor",
            vec![HostEffect::HostState(
                HostStateEffect::read::<ImpostorCache>(),
            )],
        )
    }

    let module = HostModuleDescriptor {
        name: "demo.conflict",
        functions: &[read_tuning, read_impostor],
        resources: &[],
    };
    let error = module
        .state_requirements()
        .expect_err("a conflicting state key must fail the module");
    let message = error.to_string();
    assert!(
        message.contains("demo.tuning_cache")
            && message.contains("TuningCache")
            && message.contains("ImpostorCache"),
        "diagnostic must name the conflicting key and both types: {message}"
    );

    let mut registry = HostFunctionRegistry::empty();
    let install_error = module
        .install(&mut registry)
        .expect_err("a conflicting module must not install");
    assert!(
        install_error.to_string().contains("demo.tuning_cache"),
        "install must surface the state conflict: {install_error}"
    );
    assert!(
        !registry.contains_name("demo::read_tuning")
            && !registry.contains_name("demo::read_impostor"),
        "a rejected module must leave the registry unchanged"
    );
}

#[test]
fn module_state_requirements_install_onto_a_vm_before_first_use() {
    use vm::host_api::{HostEffect, HostStateEffect};
    use vm::host_extension::HostModuleDescriptor;
    use vm::{Program, Vm};

    fn read_tuning() -> vm::host_extension::HostFunctionDescriptor {
        state_effect_descriptor(
            "demo::read_tuning",
            vec![HostEffect::HostState(HostStateEffect::read::<TuningCache>())],
        )
    }

    let module = HostModuleDescriptor {
        name: "demo.state",
        functions: &[read_tuning],
        resources: &[],
    };
    let program = Program::new(Vec::new(), vec![vm::OpCode::Ret as u8]);
    let mut vm = Vm::new(program);

    let installed = module
        .install_state_requirements(&mut vm)
        .expect("state requirements install");
    assert_eq!(installed.len(), 1);
    assert!(
        vm.host_state::<TuningCache>().is_none(),
        "installing requirements must not create the value"
    );

    let mut context = vm.host_context();
    context
        .ensure_host_state::<TuningCache>("demo::read_tuning", "state read")
        .expect("the installed provider resolves the state");
    assert!(context.host_state::<TuningCache>().is_some());
}

/// A minimal concrete resource used by the owned-dispatch descriptor tests.
#[derive(Debug)]
struct OwnedTestResource;

impl vm::resource::HostResource for OwnedTestResource {}

impl vm::host_extension::HostResourceType for OwnedTestResource {
    const KEY: &'static str = "demo.owned_resource";
    const DESCRIPTION: &'static str = "An owned-dispatch test resource";
}

fn owned_test_resource_meta() -> vm::host_extension::HostResourceTypeMeta {
    vm::host_extension::HostResourceTypeMeta::of::<OwnedTestResource>()
}

/// Owned-dispatch adapter for the descriptor tests below.
///
/// Counts dispatched calls and transfers the `TakeOwned` operand, proving the
/// descriptor routes through the owned path (operand drain + ownership
/// transfer) rather than borrowed/static dispatch.
#[derive(Default)]
struct OwnedRecorder {
    calls: usize,
}

impl vm::HostOwnedFunction for OwnedRecorder {
    fn call(&mut self, call: &mut vm::OwnedHostCall<'_>) -> vm::VmResult<vm::CallOutcome> {
        self.calls += 1;
        let transferred = call.take_arg(1)?;
        let value = match transferred {
            vm::Value::Int(value) => value,
            other => panic!("the owned argument must transfer as an int, got {other:?}"),
        };
        Ok(vm::CallOutcome::Return(vm::CallReturn::one(
            vm::Value::Int(value),
        )))
    }
}

struct OwnedRecorderFactory;

impl vm::HostOwnedAdapterFactory for OwnedRecorderFactory {
    fn create(&self, context: vm::OwnedHostContext<'_>) -> Box<dyn vm::HostOwnedFunction> {
        let _registry = context.registry();
        Box::new(OwnedRecorder::default())
    }
}

static OWNED_RECORDER_FACTORY: OwnedRecorderFactory = OwnedRecorderFactory;

fn owned_take_descriptor() -> vm::host_extension::HostFunctionDescriptor {
    use vm::host_extension::{
        HostAdapterDescriptor, HostBindingDescriptor, HostBindingKind, HostFunctionDescriptor,
    };

    let callback = HostTypeSchema::Callable {
        params: vec![HostTypeSchema::Bool],
        result: Box::new(HostTypeSchema::Unknown),
    };
    HostFunctionDescriptor {
        schema: HostFunctionSchema::with_return(
            "demo::every",
            vec![
                HostParamSchema::value("interval_ms", HostTypeSchema::Int),
                HostParamSchema::with_passing("callback", callback, HostParamPassing::TakeOwned),
            ],
            HostTypeSchema::Bool,
        )
        .with_description("Registers an owned callback."),
        binding: HostBindingDescriptor {
            kind: HostBindingKind::Owned,
        },
        effects: vec![],
        adapter: HostAdapterDescriptor::Owned(&OWNED_RECORDER_FACTORY),
        resource_types: vec![],
    }
}

#[test]
fn owned_adapter_descriptor_installs_through_the_exact_owned_registry() {
    use vm::host_extension::HostModuleDescriptor;

    let module = HostModuleDescriptor {
        name: "demo.owned",
        functions: &[owned_take_descriptor],
        resources: &[owned_test_resource_meta],
    };

    let mut registry = vm::HostFunctionRegistry::empty();
    let installed = module
        .install(&mut registry)
        .expect("an owned descriptor installs transactionally");
    assert!(registry.contains_name("demo::every"));
    assert_eq!(installed.resources().len(), 1);

    // Owned dispatch is created once per bind; registering the same
    // name+schema twice is an explicit error rather than a silent
    // replacement, and the failed install rolls the registry back.
    let error = module
        .install(&mut registry)
        .expect_err("a duplicate owned entry must fail");
    assert!(
        error.to_string().contains("demo::every"),
        "the duplicate-owner error must name the function: {error}"
    );

    // A fresh registry accepts the same module.
    let mut second = vm::HostFunctionRegistry::empty();
    registry_module()
        .install(&mut second)
        .expect("a fresh registry installs the owned module");
    assert!(second.contains_name("demo::every"));
}

fn registry_module() -> vm::host_extension::HostModuleDescriptor {
    vm::host_extension::HostModuleDescriptor {
        name: "demo.owned",
        functions: &[owned_take_descriptor],
        resources: &[],
    }
}

#[test]
fn owned_descriptor_keeps_the_take_owned_contract_and_adapter() {
    let module = registry_module();
    let catalog = module.catalog().expect("owned module catalog");
    let schemas = vm::catalog_import_schemas(&catalog, "demo::every");
    assert_eq!(
        schemas[0].params[1].passing,
        HostParamPassing::TakeOwned,
        "the owned contract must keep the take-owned passing mode"
    );

    let descriptor = (module.functions[0])();
    assert_eq!(
        descriptor.binding.kind,
        vm::host_extension::HostBindingKind::Owned,
        "an owned descriptor must not route through borrowed/static dispatch"
    );
    assert!(
        matches!(
            descriptor.adapter,
            vm::host_extension::HostAdapterDescriptor::Owned(_)
        ),
        "the owned adapter factory must be retained verbatim"
    );
}

#[test]
fn runtime_owned_pending_descriptors_mark_the_registered_import() {
    use vm::host_extension::{
        HostAdapterDescriptor, HostBindingDescriptor, HostBindingKind, HostFunctionDescriptor,
        HostModuleDescriptor,
    };

    fn pending_request_descriptor() -> HostFunctionDescriptor {
        HostFunctionDescriptor {
            schema: HostFunctionSchema::with_return(
                "demo::request",
                vec![HostParamSchema::value("request", HostTypeSchema::String)],
                HostTypeSchema::String,
            ),
            binding: HostBindingDescriptor {
                kind: HostBindingKind::StaticStackRuntimeOwned,
            },
            effects: vec![],
            adapter: HostAdapterDescriptor::StaticStackRuntimeOwned(noop_host),
            resource_types: vec![],
        }
    }

    let module = HostModuleDescriptor {
        name: "demo.pending",
        functions: &[pending_request_descriptor],
        resources: &[],
    };
    let mut registry = vm::HostFunctionRegistry::empty();
    module
        .install(&mut registry)
        .expect("a runtime-owned pending descriptor installs");
    assert!(registry.contains_name("demo::request"));

    // A mismatched binding/adapter pair still fails closed.
    let mismatched = HostFunctionDescriptor {
        binding: HostBindingDescriptor {
            kind: HostBindingKind::StaticStackRuntimeOwned,
        },
        adapter: HostAdapterDescriptor::StaticStack(noop_host),
        ..pending_request_descriptor()
    };
    let mut target = vm::HostFunctionRegistry::empty();
    let error = HostModuleDescriptor::install_descriptors(&mut target, &[mismatched])
        .expect_err("a binding/adapter mismatch must fail closed");
    assert!(
        error.to_string().contains("binding/adapter mismatch"),
        "{error}"
    );
}

#[test]
fn module_install_from_catalog_keeps_caller_identity_and_restricted_policy() {
    use vm::host_extension::HostModuleDescriptor;

    let module = HostModuleDescriptor {
        name: "demo.exact",
        functions: &[read_counter_descriptor, add_descriptor],
        resources: &[],
    };
    let catalog = module.catalog().expect("module catalog");

    // Compile-side identity: the caller catalog is authoritative, and a
    // restricted registry still receives the module's own imports.
    let mut registry = vm::HostFunctionRegistry::restricted();
    module
        .install_from_catalog(&mut registry, &catalog)
        .expect("installing against the caller catalog must succeed");
    assert!(registry.contains_name("demo::read_counter"));
    assert!(registry.contains_name("demo::add"));

    // A caller catalog that disagrees with a descriptor fails before any
    // registry mutation.
    let mut skewed = HostApiBuilder::new();
    skewed.function(HostFunctionSchema::with_return(
        "demo::add",
        vec![
            HostParamSchema::value("lhs", HostTypeSchema::Int),
            HostParamSchema::value("rhs", HostTypeSchema::Int),
        ],
        HostTypeSchema::String,
    ));
    let skewed = skewed.build().expect("skewed catalog validates");
    let mut target = vm::HostFunctionRegistry::empty();
    let error = module
        .install_from_catalog(&mut target, &skewed)
        .expect_err("a skewed caller catalog must fail the module");
    assert!(
        !target.contains_name("demo::add"),
        "a rejected module must leave the registry unchanged: {error}"
    );
}
