#![cfg(feature = "runtime")]

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use vm::resource::{HostResource, ResourceHandle};
use vm::{
    CompileSourceFileOptions, HostApiBuilder, HostFunctionSchema, HostParamPassing,
    HostParamSchema, HostTypeSchema, ResourceTypeKey, ResourceTypeSchema, SourceFlavor, TypeSchema,
    Value, Vm, VmStatus,
};

struct NeutralBox(i64);
struct ExpectUnavailable;
struct CaptureExpectations(Vec<bool>);
impl HostResource for NeutralBox {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(key())
    }
}
fn key() -> ResourceTypeKey {
    ResourceTypeKey::new("neutral.box").unwrap()
}
fn catalog() -> Arc<vm::HostApiCatalog> {
    let mut b = HostApiBuilder::new();
    b.resource(ResourceTypeSchema::new(key(), "neutral box"));
    b.function(HostFunctionSchema::with_return(
        "neutral::open",
        vec![],
        HostTypeSchema::Resource(key()),
    ));
    b.function(HostFunctionSchema::with_return(
        "neutral::probe",
        vec![HostParamSchema::with_passing(
            "candidate",
            HostTypeSchema::Resource(key()),
            HostParamPassing::Borrow,
        )],
        HostTypeSchema::Int,
    ));
    b.function(HostFunctionSchema::with_return(
        "neutral::close",
        vec![HostParamSchema::with_passing(
            "owner",
            HostTypeSchema::Resource(key()),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::Null,
    ));
    b.function(HostFunctionSchema::with_return(
        "neutral::bits",
        vec![],
        HostTypeSchema::Int,
    ));
    b.function(HostFunctionSchema::with_return(
        "neutral::check",
        vec![],
        HostTypeSchema::Int,
    ));
    Arc::new(b.build().unwrap())
}
fn source(perturbed: bool) -> String {
    format!(
        "let a = 1; let b = 2; let c = 3; let d = 4; let e = 5; let f = 6; let g = 7; let h = 8; let i = 9;\nlet owner = neutral::open();\nfn dispatch() -> int {{ neutral::probe(&owner); neutral::close(owner); 41 }}\ndispatch();\n{}",
        if perturbed {
            "fn text(value: string) -> string { let second = value; second }\nfn flag(value: bool) -> bool { let second = value; second }\ntext(\"x\"); flag(true);"
        } else {
            ""
        }
    )
}
fn bind(machine: &mut Vm, catalog: &vm::HostApiCatalog, borrows: Arc<AtomicUsize>) {
    let mut registry = vm::HostFunctionRegistry::new();
    for schema in vm::catalog_import_schemas(catalog, "neutral::open") {
        registry
            .register_exact_static("neutral::open", 0, schema, |machine, _| {
                let token = machine
                    .host_context()
                    .push_resource(NeutralBox(41))
                    .unwrap();
                Ok(vm::CallOutcome::Return(vm::CallReturn::one(Value::Int(
                    token.handle().raw() as i64,
                ))))
            })
            .unwrap();
    }
    machine.host_context().set_module_state(borrows);
    for schema in vm::catalog_import_schemas(catalog, "neutral::probe") {
        registry
            .register_exact_static(
                "neutral::probe",
                1,
                schema,
                |machine: &mut Vm, args: &[Value]| {
                    let Value::Int(raw) = args[0] else {
                        panic!("carrier")
                    };
                    let context = machine.host_context();
                    let token = context
                        .typed_resource_with_key::<NeutralBox>(
                            ResourceHandle::from_raw(raw as u64).unwrap(),
                            &key(),
                        )
                        .unwrap();
                    assert_eq!(context.resource(&token).unwrap().get().0, 41);
                    let unavailable = context
                        .module_state::<CaptureExpectations>()
                        .map(|expected| {
                            let ordinal = context
                                .module_state::<Arc<AtomicUsize>>()
                                .unwrap()
                                .load(Ordering::SeqCst);
                            !expected.0[ordinal]
                        })
                        .unwrap_or_else(|| context.module_state::<ExpectUnavailable>().is_some());
                    if unavailable {
                        let query = context.checked_move_resource_capture::<NeutralBox>(&key());
                        println!(
                            "expected unavailable: authorized={} error={:?}",
                            query.is_ok(),
                            query.as_ref().err()
                        );
                        assert!(matches!(query, Err(vm::CaptureQueryError::Unavailable)));
                    } else {
                        let checked = context
                            .checked_move_resource_capture::<NeutralBox>(&key())
                            .unwrap();
                        assert_eq!(checked.token().handle(), token.handle());
                        assert_eq!(checked.root().observe(&context), vm::CaptureRootState::Live);
                    }
                    context
                        .module_state::<Arc<AtomicUsize>>()
                        .unwrap()
                        .fetch_add(1, Ordering::SeqCst);
                    Ok(vm::CallOutcome::Return(vm::CallReturn::one(Value::Int(41))))
                },
            )
            .unwrap();
    }
    for schema in vm::catalog_import_schemas(catalog, "neutral::close") {
        registry
            .register_exact_static("neutral::close", 1, schema, |machine, args| {
                let Value::Int(raw) = args[0] else {
                    panic!("carrier")
                };
                machine
                    .host_context()
                    .close_resource::<NeutralBox>(
                        ResourceHandle::from_raw(raw as u64).unwrap(),
                        vm::resource::ResourceCloseReason::Requested,
                    )
                    .unwrap();
                Ok(vm::CallOutcome::Return(vm::CallReturn::none()))
            })
            .unwrap();
    }
    registry.bind_vm_cached(machine).unwrap();
}

#[test]
fn rss_legal_resource_capture_aggregate_schema_perturbation() {
    let catalog = catalog();
    for perturbed in [false, true] {
        let compiled = vm::compile_source_with_flavor_and_options_with_capture_certification(
            &source(perturbed),
            SourceFlavor::RustScript,
            CompileSourceFileOptions::new().with_host_api_catalog(catalog.clone()),
        )
        .unwrap();
        let ordinary = vm::compile_source_with_flavor_and_options(
            &source(perturbed),
            SourceFlavor::RustScript,
            CompileSourceFileOptions::new().with_host_api_catalog(catalog.clone()),
        )
        .unwrap();
        let prototype = ordinary
            .program
            .callable_prototypes
            .iter()
            .find(|p| p.capture_modes.contains(&vm::CaptureBindingMode::Move))
            .unwrap();
        let ordinal = prototype
            .capture_modes
            .iter()
            .position(|mode| *mode == vm::CaptureBindingMode::Move)
            .unwrap();
        let slot = prototype.capture_source_slots[ordinal] as usize;
        let aggregate = ordinary.program.type_map.as_ref().unwrap().local_schemas[slot].clone();
        println!(
            "perturbed={perturbed} source={slot} target={} aggregate={aggregate:?}",
            prototype.capture_slots[ordinal]
        );
        let borrows = Arc::new(AtomicUsize::new(0));
        let mut machine = compiled.into_vm().unwrap();
        bind(&mut machine, &catalog, borrows.clone());
        assert_eq!(machine.run().unwrap(), VmStatus::Halted);
        assert_eq!(borrows.load(Ordering::SeqCst), 1);
        if perturbed {
            assert_eq!(
                aggregate,
                Some(TypeSchema::Bool),
                "aggregate remains an unsuitable owner guard"
            );
        }
    }
}

struct NumericControl {
    handle: ResourceHandle,
    calls: usize,
}

struct MoveNumberFrontend;
impl vm::SourcePlugin for MoveNumberFrontend {
    fn descriptor(&self) -> vm::SourceFrontendDescriptor {
        vm::SourceFrontendDescriptor {
            id: vm::FrontendId::new("neutral-move-number"),
            extensions: &["move-number"],
            typing_policy: vm::FrontendTypingPolicy::Strict,
            supports_ownership_checking: true,
        }
    }
    fn parse_source(
        &self,
        source: &str,
        context: &vm::SourceFrontendContext<'_>,
    ) -> Result<vm::FrontendIr, vm::ParseError> {
        let mut ir = parse_neutral(source, context)?;
        for implementation in ir.function_impls.values_mut() {
            if let vm::Expr::Var(slot) = implementation.body_expr {
                implementation.body_expr = vm::Expr::MoveVar(slot);
            }
        }
        Ok(ir)
    }
}
struct NeutralDialect;
impl vm::ParserDialect for NeutralDialect {}
fn parse_neutral(
    source: &str,
    context: &vm::SourceFrontendContext<'_>,
) -> Result<vm::FrontendIr, vm::ParseError> {
    let mut ir = vm::parse_source_with_dialect(
        source,
        &NeutralDialect,
        vm::SharedParserOptions {
            source_id: context.source_id,
            allow_implicit_externs: true,
            import_scan_mode: true,
            enforce_mutable_bindings: true,
            ..Default::default()
        },
    )?;
    let placeholders = ir
        .functions
        .iter()
        .filter(|f| {
            ir.implicit_extern_names.contains(&f.name)
                && matches!(
                    f.name.as_str(),
                    "open" | "probe" | "close" | "bits" | "check"
                )
        })
        .cloned()
        .collect::<Vec<_>>();
    ir.functions
        .retain(|f| !placeholders.iter().any(|p| p.index == f.index));
    ir.implicit_extern_names
        .retain(|name| !placeholders.iter().any(|p| &p.name == name));
    let mut mapping = std::collections::HashMap::new();
    for old in placeholders {
        mapping.insert(
            old.index,
            context.install_host_function(&mut ir, &format!("neutral::{}", old.name), old.arity)?,
        );
    }
    fn remap(expr: &mut vm::Expr, mapping: &std::collections::HashMap<u16, u16>) {
        if let vm::Expr::Call(index, _, arguments, ..) = expr {
            if let Some(new) = mapping.get(index) {
                *index = *new;
            }
            for arg in arguments {
                remap(arg, mapping);
            }
        }
        match expr {
            vm::Expr::IfElse {
                condition,
                then_expr,
                else_expr,
            } => {
                remap(condition, mapping);
                remap(then_expr, mapping);
                remap(else_expr, mapping);
            }
            vm::Expr::Closure(closure) | vm::Expr::ClosureCall(closure, _) => {
                remap(&mut closure.body, mapping);
            }
            vm::Expr::Block { stmts, expr } => {
                for stmt in stmts {
                    if let vm::Stmt::Let { expr, .. } | vm::Stmt::Expr { expr, .. } = stmt {
                        remap(expr, mapping);
                    }
                }
                remap(expr, mapping);
            }
            _ => {}
        }
    }
    for statement in &mut ir.stmts {
        if let vm::Stmt::Let { expr, .. } | vm::Stmt::Expr { expr, .. } = statement {
            remap(expr, &mapping);
        }
    }
    for implementation in ir.function_impls.values_mut() {
        for statement in &mut implementation.body_stmts {
            if let vm::Stmt::Let { expr, .. } | vm::Stmt::Expr { expr, .. } = statement {
                remap(expr, &mapping);
            }
        }
        remap(&mut implementation.body_expr, &mapping);
    }
    Ok(ir)
}
static MOVE_NUMBER: MoveNumberFrontend = MoveNumberFrontend;

struct BoxNotesFrontend;
static BOX_NOTES: BoxNotesFrontend = BoxNotesFrontend;
impl vm::SourcePlugin for BoxNotesFrontend {
    fn descriptor(&self) -> vm::SourceFrontendDescriptor {
        vm::SourceFrontendDescriptor {
            id: vm::FrontendId::new("neutral-box-notes"),
            extensions: &["box-notes"],
            typing_policy: vm::FrontendTypingPolicy::Strict,
            supports_ownership_checking: true,
        }
    }
    fn parse_source(
        &self,
        source: &str,
        context: &vm::SourceFrontendContext<'_>,
    ) -> Result<vm::FrontendIr, vm::ParseError> {
        if source.trim() != "box dispatch" {
            return Err(vm::ParseError::new("expected box dispatch"));
        }
        parse_neutral(
            "let owner = open(); fn dispatch() -> int { probe(&owner); close(owner); 41 } dispatch();",
            context,
        )
    }
}

struct NonOwningFrontend;
static NON_OWNING: NonOwningFrontend = NonOwningFrontend;
impl vm::SourcePlugin for NonOwningFrontend {
    fn descriptor(&self) -> vm::SourceFrontendDescriptor {
        vm::SourceFrontendDescriptor {
            id: vm::FrontendId::new("neutral-non-owning"),
            extensions: &["non-owning"],
            typing_policy: vm::FrontendTypingPolicy::Strict,
            supports_ownership_checking: false,
        }
    }
    fn parse_source(
        &self,
        source: &str,
        context: &vm::SourceFrontendContext<'_>,
    ) -> Result<vm::FrontendIr, vm::ParseError> {
        parse_neutral(&source.replace("use self::dep;", "              "), context)
    }
    fn parse_module_imports(
        &self,
        source: &str,
        _path: &std::path::Path,
        _context: &vm::SourceFrontendContext<'_>,
    ) -> Result<Vec<vm::ModuleImport>, vm::SourcePathError> {
        Ok(if source.starts_with("use self::dep;") {
            vec![vm::ModuleImport::value(
                "./dep.rss",
                vm::ImportClause::Named(vec![vm::NamedImport::value(
                    if source.contains("forward()") {
                        "forward"
                    } else {
                        "dispatch"
                    },
                    if source.contains("forward()") {
                        "forward"
                    } else {
                        "dispatch"
                    },
                )]),
                1,
            )]
        } else {
            Vec::new()
        })
    }
}

#[test]
fn ownership_disabled_frontend_executes_but_cannot_certify_capture_origin() {
    let catalog = catalog();
    let issued = vm::compile_source_with_flavor_and_options_with_capture_certification(
        "let owner = open(); fn dispatch() -> int { probe(owner); close(owner); 41 } dispatch();",
        SourceFlavor::Plugin(vm::SourcePlugin::descriptor(&NON_OWNING).id),
        CompileSourceFileOptions::new()
            .with_source_plugin(&NON_OWNING)
            .with_host_api_catalog(catalog.clone()),
    )
    .unwrap();
    let mut machine = issued.into_vm().unwrap();
    machine.host_context().set_module_state(ExpectUnavailable);
    let borrows = Arc::new(AtomicUsize::new(0));
    bind(&mut machine, &catalog, borrows.clone());
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    assert_eq!(borrows.load(Ordering::SeqCst), 1);
    assert_eq!(machine.host_context().resource_count(), 0);
    assert_eq!(
        machine.capture_certification_stats().unwrap().registrations,
        0
    );
}

#[test]
fn mixed_policy_disabled_dependency_executes_without_capture_certification() {
    let catalog = catalog();
    let source = "use self::dep; dep::dispatch();";
    let options = || {
        CompileSourceFileOptions::new()
            .with_source_plugin(&NON_OWNING)
            .with_host_api_catalog(catalog.clone())
            .with_module_override_path("dep.rss", "dep.non-owning")
            .with_module_override_source(
                "dep.rss",
                "let owner=open(); pub fn dispatch()->int {probe(owner);close(owner);41}",
            )
    };
    let ordinary =
        vm::compile_source_with_flavor_and_options(source, SourceFlavor::RustScript, options())
            .unwrap();
    for prototype in &ordinary.program.callable_prototypes {
        println!(
            "prototype source={:?} target={:?} modes={:?}",
            prototype.capture_source_slots, prototype.capture_slots, prototype.capture_modes
        );
    }
    let mut ordinary_machine = ordinary.into_vm().unwrap();
    ordinary_machine
        .host_context()
        .set_module_state(ExpectUnavailable);
    let ordinary_borrows = Arc::new(AtomicUsize::new(0));
    bind(&mut ordinary_machine, &catalog, ordinary_borrows.clone());
    assert_eq!(ordinary_machine.run().unwrap(), VmStatus::Halted);
    assert_eq!(ordinary_machine.stack(), &[Value::Int(41)]);
    assert_eq!(ordinary_borrows.load(Ordering::SeqCst), 1);
    assert_eq!(ordinary_machine.host_context().resource_count(), 0);
    println!("ordinary mixed execution: result=41 probes=1 resources=0");
    let issued = vm::compile_source_with_flavor_and_options_with_capture_certification(
        source,
        SourceFlavor::RustScript,
        options(),
    )
    .unwrap();
    let mut machine = issued.into_vm().unwrap();
    machine.host_context().set_module_state(ExpectUnavailable);
    let borrows = Arc::new(AtomicUsize::new(0));
    bind(&mut machine, &catalog, borrows.clone());
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    assert_eq!(borrows.load(Ordering::SeqCst), 1);
    assert_eq!(machine.host_context().resource_count(), 0);
    assert_eq!(
        machine.capture_certification_stats().unwrap().registrations,
        0
    );
}

fn assert_mixed_producer(
    dependency: &str,
    dependency_enabled: bool,
    entry_enabled: bool,
    transitive: bool,
) {
    assert_mixed_producer_return(
        dependency,
        dependency_enabled,
        entry_enabled,
        transitive,
        false,
    );
}

fn assert_mixed_producer_return(
    dependency: &str,
    dependency_enabled: bool,
    entry_enabled: bool,
    transitive: bool,
    returns_callable: bool,
) {
    let catalog = catalog();
    let entry = match (entry_enabled, transitive) {
        (true, true) => "use self::dep; dep::forward();",
        (true, false) => "use self::dep; dep::dispatch();",
        (false, true) => "use self::dep; forward();",
        (false, false) => "use self::dep; dispatch();",
    };
    let flavor = if entry_enabled {
        SourceFlavor::RustScript
    } else {
        SourceFlavor::Plugin(vm::SourcePlugin::descriptor(&NON_OWNING).id)
    };
    let entry = if returns_callable {
        if entry_enabled {
            "use self::dep; let callback=dep::dispatch(); callback();"
        } else {
            "use self::dep; let callback=dispatch(); callback();"
        }
    } else {
        entry
    };
    let options = || {
        let mut options = CompileSourceFileOptions::new()
            .with_source_plugin(&NON_OWNING)
            .with_host_api_catalog(catalog.clone());
        let producer = if transitive { "leaf.rss" } else { "dep.rss" };
        if !dependency_enabled {
            options = options.with_module_override_path(producer, "producer.non-owning");
        }
        options = options.with_module_override_source(producer, dependency);
        if transitive {
            options = options.with_module_override_source(
                "dep.rss",
                "use self::leaf; pub fn forward() -> int { leaf::dispatch() }",
            );
        }
        options
    };
    let ordinary = vm::compile_source_with_flavor_and_options(entry, flavor, options()).unwrap();
    let issued = vm::compile_source_with_flavor_and_options_with_capture_certification(
        entry,
        flavor,
        options(),
    )
    .unwrap();
    let paired = vm::compile_source_with_flavor_and_options_with_capture_certification(
        entry,
        flavor,
        options(),
    )
    .unwrap()
    .into_uncertified();
    assert_eq!(
        vm::vmbc::encode_program(&ordinary.program).unwrap(),
        vm::vmbc::encode_program(&paired.program).unwrap()
    );
    for (mut machine, unavailable) in [
        (ordinary.into_vm().unwrap(), true),
        (issued.into_vm().unwrap(), !dependency_enabled),
    ] {
        if unavailable {
            machine.host_context().set_module_state(ExpectUnavailable);
        }
        let borrows = Arc::new(AtomicUsize::new(0));
        bind(&mut machine, &catalog, borrows.clone());
        assert_eq!(machine.run().unwrap(), VmStatus::Halted);
        assert_eq!(machine.stack(), &[Value::Int(41)]);
        assert_eq!(borrows.load(Ordering::SeqCst), 1);
        assert_eq!(machine.host_context().resource_count(), 0);
        if let Some(stats) = machine.capture_certification_stats() {
            assert_eq!(stats.registrations > 0, dependency_enabled);
        }
    }
}

#[test]
fn mixed_policy_root_anonymous_dependency_follows_producer() {
    for entry_enabled in [true, false] {
        assert_mixed_producer(
            "let owner=open(); let callback=|| if true => {probe(owner);close(owner);41} else => {0}; let finished=callback(); pub fn dispatch()->int {41}",
            false,
            entry_enabled,
            false,
        );
        assert_mixed_producer(
            "let owner=neutral::open(); let callback=|| if true => {neutral::probe(&owner);neutral::close(owner);41} else => {0}; let finished=callback(); pub fn dispatch()->int {41}",
            true,
            entry_enabled,
            false,
        );
    }
}

#[test]
fn mixed_policy_nested_anonymous_dependency_follows_producer() {
    for entry_enabled in [true, false] {
        assert_mixed_producer(
            "pub fn dispatch()->int {let outer=|| if true => {let owner=open();let inner=|| if true => {probe(owner);close(owner);41} else => {0};inner()} else => {0};outer()}",
            false,
            entry_enabled,
            false,
        );
        assert_mixed_producer(
            "pub fn dispatch()->int {let outer=|| if true => {let owner=neutral::open();let inner=|| if true => {neutral::probe(&owner);neutral::close(owner);41} else => {0};inner()} else => {0};outer()}",
            true,
            entry_enabled,
            false,
        );
    }
}

#[test]
fn mixed_policy_returned_closure_keeps_factory_producer() {
    for entry_enabled in [true, false] {
        assert_mixed_producer_return(
            "pub fn dispatch()->fn()->int {let owner=open(); || if true => {probe(owner);close(owner);41} else => {0}}",
            false,
            entry_enabled,
            false,
            true,
        );
        assert_mixed_producer_return(
            "pub fn dispatch()->fn()->int {let owner=neutral::open(); || if true => {neutral::probe(&owner);neutral::close(owner);41} else => {0}}",
            true,
            entry_enabled,
            false,
            true,
        );
    }
}

#[test]
fn mixed_policy_named_enabled_dependency_survives_disabled_entry() {
    for entry_enabled in [true, false] {
        assert_mixed_producer(
            "let owner=neutral::open(); pub fn dispatch()->int {neutral::probe(&owner);neutral::close(owner);41}",
            true,
            entry_enabled,
            false,
        );
    }
}

#[test]
fn mixed_policy_transitive_dependency_follows_producer() {
    for entry_enabled in [true, false] {
        assert_mixed_producer(
            "let owner=open(); pub fn dispatch()->int {probe(owner);close(owner);41}",
            false,
            entry_enabled,
            true,
        );
        assert_mixed_producer(
            "let owner=neutral::open(); pub fn dispatch()->int {neutral::probe(&owner);neutral::close(owner);41}",
            true,
            entry_enabled,
            true,
        );
    }
}

#[test]
fn mixed_policy_root_closures_keep_distinct_producers_after_compaction() {
    let catalog = catalog();
    let entry = "use self::dep; let ignored=dep::dispatch(); let a=1;let b=2;let c=3;let d=4;let e=5;let f=6;let g=7;let h=8;let i=9; let owner=neutral::open();let callback=|| if true => {neutral::probe(&owner);neutral::close(owner);41} else => {0};callback();";
    let options = || {
        CompileSourceFileOptions::new()
        .with_source_plugin(&NON_OWNING)
        .with_host_api_catalog(catalog.clone())
        .with_module_override_path("dep.rss", "dep.non-owning")
        .with_module_override_source("dep.rss", "let owner=open();let callback=|| if true => {probe(owner);close(owner);41} else => {0};let finished=callback();pub fn dispatch()->int {41}")
    };
    let ordinary =
        vm::compile_source_with_flavor_and_options(entry, SourceFlavor::RustScript, options())
            .unwrap();
    let issued = vm::compile_source_with_flavor_and_options_with_capture_certification(
        entry,
        SourceFlavor::RustScript,
        options(),
    )
    .unwrap();
    let paired = vm::compile_source_with_flavor_and_options_with_capture_certification(
        entry,
        SourceFlavor::RustScript,
        options(),
    )
    .unwrap()
    .into_uncertified();
    assert_eq!(
        vm::vmbc::encode_program(&ordinary.program).unwrap(),
        vm::vmbc::encode_program(&paired.program).unwrap()
    );
    println!(
        "compacted producer layouts: {:?}",
        ordinary
            .program
            .callable_prototypes
            .iter()
            .map(|p| (&p.capture_source_slots, &p.capture_slots, &p.capture_modes))
            .collect::<Vec<_>>()
    );
    for (mut machine, expected) in [
        (ordinary.into_vm().unwrap(), vec![false, false]),
        (issued.into_vm().unwrap(), vec![false, true]),
    ] {
        machine
            .host_context()
            .set_module_state(CaptureExpectations(expected));
        let borrows = Arc::new(AtomicUsize::new(0));
        bind(&mut machine, &catalog, borrows.clone());
        assert_eq!(machine.run().unwrap(), VmStatus::Halted);
        assert_eq!(machine.stack(), &[Value::Int(41)]);
        assert_eq!(borrows.load(Ordering::SeqCst), 2);
        assert_eq!(machine.host_context().resource_count(), 0);
        if let Some(stats) = machine.capture_certification_stats() {
            assert_eq!(stats.registrations, 1);
        }
    }
}

#[test]
fn mixed_policy_ordinary_program_layout_control() {
    let catalog = catalog();
    for entry_enabled in [true, false] {
        for dependency_enabled in [true, false] {
            let entry = if entry_enabled {
                "use self::dep; dep::dispatch();"
            } else {
                "use self::dep; dispatch();"
            };
            let flavor = if entry_enabled {
                SourceFlavor::RustScript
            } else {
                SourceFlavor::Plugin(vm::SourcePlugin::descriptor(&NON_OWNING).id)
            };
            let dependency = if dependency_enabled {
                "let seed=1; if seed==1 {let a=2;let b=3;let c=4;a+b+c;} let owner=neutral::open(); let callback=|| if true => {neutral::probe(&owner);neutral::close(owner);41} else => {0}; let finished=callback(); pub fn dispatch()->int {41}"
            } else {
                "let owner=open(); pub fn dispatch()->int {probe(owner);close(owner);41}"
            };
            let options = || {
                let options = CompileSourceFileOptions::new()
                    .with_source_plugin(&NON_OWNING)
                    .with_host_api_catalog(catalog.clone())
                    .with_module_override_source("dep.rss", dependency);
                if dependency_enabled {
                    options
                } else {
                    options.with_module_override_path("dep.rss", "producer.non-owning")
                }
            };
            let ordinary =
                vm::compile_source_with_flavor_and_options(entry, flavor, options()).unwrap();
            let paired = vm::compile_source_with_flavor_and_options_with_capture_certification(
                entry,
                flavor,
                options(),
            )
            .unwrap()
            .into_uncertified();
            let wire = vm::vmbc::encode_program(&ordinary.program).unwrap();
            assert_eq!(wire, vm::vmbc::encode_program(&paired.program).unwrap());
            if let Some(output) = std::env::var_os("PROVENANCE_POLICY_PROGRAMS") {
                std::fs::write(
                    std::path::Path::new(&output).join(format!(
                        "entry-{entry_enabled}-dependency-{dependency_enabled}.vmbc"
                    )),
                    &wire,
                )
                .unwrap();
            }
            let mut machine = ordinary.into_vm().unwrap();
            machine.host_context().set_module_state(ExpectUnavailable);
            let borrows = Arc::new(AtomicUsize::new(0));
            bind(&mut machine, &catalog, borrows.clone());
            assert_eq!(machine.run().unwrap(), VmStatus::Halted);
            assert_eq!(machine.stack().last(), Some(&Value::Int(41)));
            assert_eq!(borrows.load(Ordering::SeqCst), 1);
            assert_eq!(machine.host_context().resource_count(), 0);
        }
    }
}

#[test]
fn two_independent_typed_frontends_admit_and_execute_resource_capture() {
    let catalog = catalog();
    for (plugin, input) in [
        (
            &MOVE_NUMBER as &'static dyn vm::SourcePlugin,
            "let owner = open(); fn dispatch() -> int { probe(&owner); close(owner); 41 } dispatch();",
        ),
        (&BOX_NOTES as &'static dyn vm::SourcePlugin, "box dispatch"),
    ] {
        let certified = vm::compile_source_with_flavor_and_options_with_capture_certification(
            input,
            SourceFlavor::Plugin(plugin.descriptor().id),
            CompileSourceFileOptions::new()
                .with_source_plugin(plugin)
                .with_host_api_catalog(catalog.clone()),
        )
        .unwrap();
        let mut machine = certified.into_vm().unwrap();
        let borrows = Arc::new(AtomicUsize::new(0));
        bind(&mut machine, &catalog, borrows.clone());
        assert_eq!(machine.run().unwrap(), VmStatus::Halted);
        assert_eq!(borrows.load(Ordering::SeqCst), 1);
        assert_eq!(
            machine.capture_certification_stats().unwrap().registrations,
            1
        );
        assert_eq!(machine.host_context().resource_count(), 0);
    }
}

#[test]
fn source_file_certification_matches_ordinary_options_and_old_three_field_literals() {
    let catalog = catalog();
    let path = std::env::temp_dir().join(format!("capture-public-{}.rss", std::process::id()));
    std::fs::write(&path, source(true)).unwrap();
    let options = || CompileSourceFileOptions::new().with_host_api_catalog(catalog.clone());
    let ordinary = vm::compile_source_file_with_options(&path, options()).unwrap();
    let vm::CompiledProgram {
        program,
        locals,
        functions,
    } = ordinary;
    let literal = vm::CompiledProgram {
        program,
        locals,
        functions,
    };
    let certified =
        vm::compile_source_file_with_capture_certification_and_options(&path, options()).unwrap();
    assert_eq!(
        vm::vmbc::encode_program(&literal.program).unwrap(),
        vm::vmbc::encode_program(&certified.into_uncertified().program).unwrap()
    );
    let certified =
        vm::compile_source_file_with_capture_certification_and_options(&path, options()).unwrap();
    let mut machine = certified.into_vm().unwrap();
    let borrows = Arc::new(AtomicUsize::new(0));
    bind(&mut machine, &catalog, borrows.clone());
    machine.run().unwrap();
    assert_eq!(borrows.load(Ordering::SeqCst), 1);
    std::fs::remove_file(path).unwrap();
    let issued = vm::compile_source_with_capture_certification("40 + 2;").unwrap();
    let ordinary = vm::compile_source("40 + 2;").unwrap();
    assert_eq!(
        vm::vmbc::encode_program(&issued.into_uncertified().program).unwrap(),
        vm::vmbc::encode_program(&ordinary.program).unwrap()
    );
}

#[test]
fn moved_int_equal_to_live_typed_handle_has_no_resource_origin() {
    let catalog = catalog();
    let source = "let owner = bits(); fn dispatch() -> int { check(); owner } dispatch();";
    let flavor = SourceFlavor::Plugin(vm::FrontendId::new("neutral-move-number"));
    let options = || {
        CompileSourceFileOptions::new()
            .with_source_plugin(&MOVE_NUMBER)
            .with_host_api_catalog(catalog.clone())
    };
    let ordinary = vm::compile_source_with_flavor_and_options(source, flavor, options()).unwrap();
    assert!(
        ordinary
            .program
            .callable_prototypes
            .iter()
            .any(|p| p.capture_modes == [vm::CaptureBindingMode::Move])
    );
    let certified = vm::compile_source_with_flavor_and_options_with_capture_certification(
        source,
        flavor,
        options(),
    )
    .unwrap();
    let mut machine = certified.into_vm().unwrap();
    let mut registry = vm::HostFunctionRegistry::new();
    for schema in vm::catalog_import_schemas(&catalog, "neutral::bits") {
        registry
            .register_exact_static("neutral::bits", 0, schema, |machine, _| {
                let token = machine
                    .host_context()
                    .push_resource(NeutralBox(41))
                    .unwrap();
                machine.host_context().set_module_state(NumericControl {
                    handle: token.handle(),
                    calls: 0,
                });
                Ok(vm::CallOutcome::Return(vm::CallReturn::one(Value::Int(
                    token.handle().raw() as i64,
                ))))
            })
            .unwrap();
    }
    for schema in vm::catalog_import_schemas(&catalog, "neutral::check") {
        registry
            .register_exact_static("neutral::check", 0, schema, |machine, _| {
                let mut context = machine.host_context();
                assert!(matches!(
                    context.checked_move_resource_capture::<NeutralBox>(&key()),
                    Err(vm::CaptureQueryError::NoMatchingCapture)
                ));
                let handle = context.module_state::<NumericControl>().unwrap().handle;
                let candidate = context
                    .typed_resource_with_key::<NeutralBox>(handle, &key())
                    .unwrap();
                assert_eq!(context.resource(&candidate).unwrap().get().0, 41);
                context.module_state_mut::<NumericControl>().unwrap().calls += 1;
                Ok(vm::CallOutcome::Return(vm::CallReturn::one(Value::Int(0))))
            })
            .unwrap();
    }
    registry.bind_vm_cached(&mut machine).unwrap();
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    assert_eq!(
        machine
            .host_context()
            .module_state::<NumericControl>()
            .unwrap()
            .calls,
        1
    );
    assert_eq!(
        machine.capture_certification_stats().unwrap().registrations,
        0
    );
}
