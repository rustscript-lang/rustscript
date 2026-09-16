#![cfg(feature = "runtime")]

//! Characterization and behavior tests for host function descriptors,
//! guest resource effects, deterministic module aggregation, and
//! transactional installation.

use std::sync::Arc;

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
    assert_eq!(catalog.fingerprint().to_string(), "b8b323eb678acb33");
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
    assert_eq!(catalog.fingerprint().to_string(), "1ae5b55fa213c708");
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
    assert_eq!(catalog.fingerprint().to_string(), "3feba549802b8453");
}

#[test]
fn mutable_resource_catalog_uses_borrow_mut_passing() {
    let catalog = mutable_resource_catalog();
    let schemas = catalog_import_schemas(&catalog, "demo::bump_counter");
    assert_eq!(schemas[0].params[0].passing, HostParamPassing::BorrowMut);
    let rebuilt = mutable_resource_catalog();
    assert_eq!(catalog.fingerprint(), rebuilt.fingerprint());
    assert_eq!(catalog.fingerprint().to_string(), "5e72bc2ba4ad83c4");
}

#[test]
fn owned_resource_catalog_uses_take_owned_passing() {
    let catalog = owned_resource_catalog();
    let schemas = catalog_import_schemas(&catalog, "demo::take_counter");
    assert_eq!(schemas[0].params[0].passing, HostParamPassing::TakeOwned);
    let rebuilt = owned_resource_catalog();
    assert_eq!(catalog.fingerprint(), rebuilt.fingerprint());
    assert_eq!(catalog.fingerprint().to_string(), "0b4c23d5c9d8dbcd");
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
    assert_eq!(catalog.fingerprint().to_string(), "b2ff5d0e37985029");
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
    assert_eq!(catalog.fingerprint().to_string(), "3feba549802b8453");
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

    let module = HostModuleDescriptor {
        name: "demo",
        functions: &[read_counter_descriptor, add_descriptor],
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
        "partial install must not leak the later function"
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

#[allow(dead_code)]
fn _keep_arc_import_for_later_descriptor_install(catalog: HostApiCatalog) -> Arc<HostApiCatalog> {
    Arc::new(catalog)
}
