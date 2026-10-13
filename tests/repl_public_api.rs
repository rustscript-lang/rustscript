use vm::compiler::TypeSchema;
use vm::{ReplLocalBinding, ReplLocalState, compile_source_for_repl_with_state};

#[test]
fn root_result_join_optional_float_uses_expression_schema() {
    let compiled = vm::compile_source_for_repl_with_state_and_options(
        "let maybe: float? = 2.5; let y = if true => { maybe } else => { null };",
        &[],
        vm::CompileSourceFileOptions::default(),
    )
    .unwrap();
    let binding = compiled.bindings.iter().find(|b| b.name == "y").unwrap();
    println!("root result join: y={binding:?}");
    #[cfg(feature = "runtime")]
    {
        let mut machine = vm::Vm::new(compiled.compiled.program.clone());
        assert_eq!(machine.run().unwrap(), vm::VmStatus::Halted);
        let slot = machine.debug_info().unwrap().local_index("y").unwrap();
        assert_eq!(machine.locals()[slot as usize], vm::Value::Float(2.5));
        println!(
            "root result join: runtime y={:?}",
            machine.locals()[slot as usize]
        );
    }
    assert_eq!(binding.schema, Some(TypeSchema::Float));
    assert!(binding.optional);
    let states = compiled
        .bindings
        .into_iter()
        .map(|binding| ReplLocalState {
            binding,
            moved: false,
        })
        .collect::<Vec<_>>();
    let next = vm::compile_source_for_repl_with_state_and_options(
        "let following = y; following;",
        &states,
        vm::CompileSourceFileOptions::default(),
    )
    .unwrap();
    let following = next
        .bindings
        .iter()
        .find(|b| b.name == "following")
        .unwrap();
    assert_eq!(following.schema, Some(TypeSchema::Float));
    assert!(following.optional);
    #[cfg(feature = "runtime")]
    assert_eq!(
        execute_persistent_snippets(
            &[
                "let maybe: float? = 2.5; let y = if true => { maybe } else => { null };",
                "let following = y; following;",
            ],
            vm::CompileSourceFileOptions::default(),
        ),
        vec![vm::Value::Float(2.5)]
    );
}

#[test]
fn root_result_join_optional_named_retains_checked_host_identity() {
    let named = vm::HostStructSchema::new(
        "ResultJoinRoot",
        vec![vm::HostStructField::new("value", vm::HostTypeSchema::Int)],
    );
    let optional_named = vm::HostTypeSchema::Optional(Box::new(named.as_type()));
    let mut builder = vm::HostApiBuilder::new();
    builder.named_struct(named.clone());
    builder.function(vm::HostFunctionSchema::with_return(
        "resultjoin::origin",
        vec![],
        optional_named,
    ));
    let catalog = std::sync::Arc::new(builder.build().unwrap());
    let options = vm::CompileSourceFileOptions::default()
        .with_host_api_catalog(std::sync::Arc::clone(&catalog));
    #[cfg(feature = "runtime")]
    let registry = {
        fn origin(_: &mut vm::Vm, _: &[vm::Value]) -> vm::VmResult<vm::CallOutcome> {
            Ok(vm::CallOutcome::Return(vm::return_one(vm::Value::map(
                vec![(vm::Value::string("value"), vm::Value::Int(42))],
            ))))
        }
        let mut registry = vm::HostFunctionRegistry::new();
        registry
            .register_catalog_static(
                vm::catalog_import_schemas(&catalog, "resultjoin::origin").remove(0),
                origin,
            )
            .unwrap();
        registry
    };
    for (expression, flag, expected) in [
        ("if flag => { maybe } else => { null }", true, 42),
        ("if flag => { null } else => { maybe }", false, 42),
        ("if flag => { maybe } else => { null }", false, 0),
        ("if flag => { null } else => { maybe }", true, 0),
        ("match choice { 1 => maybe, _ => null }", true, 42),
        ("match choice { 1 => null, _ => maybe }", false, 42),
        ("match choice { 1 => maybe, _ => null }", false, 0),
        ("match choice { 1 => null, _ => maybe }", true, 0),
    ] {
        let source = format!(
            "let flag = {flag}; let choice = if flag => {{ 1 }} else => {{ 2 }}; let maybe: ResultJoinRoot? = resultjoin::origin(); let y = {expression};"
        );
        let first =
            vm::compile_source_for_repl_with_state_and_options(&source, &[], options.clone())
                .unwrap();
        let y = first.bindings.iter().find(|b| b.name == "y").unwrap();
        println!("root result join: {expression}, y={y:?}");
        assert_eq!(
            y.schema,
            Some(TypeSchema::Named("ResultJoinRoot".into(), vec![]))
        );
        assert!(y.optional);
        let states = first
            .bindings
            .iter()
            .cloned()
            .map(|binding| ReplLocalState {
                binding,
                moved: false,
            })
            .collect::<Vec<_>>();
        let second = vm::compile_source_for_repl_with_state_and_options(
            "let following = y; following == null;",
            &states,
            options.clone(),
        )
        .unwrap();
        let following = second
            .bindings
            .iter()
            .find(|b| b.name == "following")
            .unwrap();
        assert_eq!(following.schema, y.schema);
        assert!(following.optional);
        #[cfg(feature = "runtime")]
        {
            let mut machine = vm::Vm::new(first.compiled.program);
            registry.bind_vm_cached(&mut machine).unwrap();
            assert_eq!(machine.run().unwrap(), vm::VmStatus::Halted);
            let mut next = vm::Vm::new(second.compiled.program);
            registry.bind_vm_cached(&mut next).unwrap();
            for binding in &first.bindings {
                let previous_slot = machine
                    .debug_info()
                    .unwrap()
                    .local_index(&binding.name)
                    .unwrap();
                let slot = next
                    .debug_info()
                    .unwrap()
                    .local_index(&binding.name)
                    .unwrap();
                next.set_local(slot, machine.locals()[previous_slot as usize].clone())
                    .unwrap();
            }
            let y_slot = machine.debug_info().unwrap().local_index("y").unwrap();
            if expected == 0 {
                assert_eq!(machine.locals()[y_slot as usize], vm::Value::Null);
            } else {
                let vm::Value::Map(fields) = &machine.locals()[y_slot as usize] else {
                    panic!("host result carrier")
                };
                assert_eq!(
                    fields.get(&vm::Value::string("value")),
                    Some(&vm::Value::Int(expected))
                );
            }
            assert_eq!(next.run().unwrap(), vm::VmStatus::Halted);
            assert_eq!(next.stack(), &[vm::Value::Bool(expected == 0)]);
        }
        #[cfg(not(feature = "runtime"))]
        let _ = expected;
    }
}

#[test]
fn root_result_join_match_uses_existing_expression_alternatives() {
    for expression in [
        "match 1 { 1 => maybe, _ => null }",
        "match 2 { 1 => null, _ => maybe }",
    ] {
        let source = format!("let maybe: float? = 2.5; let y = {expression};");
        let compiled = vm::compile_source_for_repl_with_state_and_options(
            &source,
            &[],
            vm::CompileSourceFileOptions::default(),
        )
        .unwrap();
        let y = compiled.bindings.iter().find(|b| b.name == "y").unwrap();
        println!("root result join: {expression}, y={y:?}");
        assert_eq!(y.schema, Some(TypeSchema::Float));
        assert!(y.optional);
        #[cfg(feature = "runtime")]
        assert_eq!(
            execute_persistent_snippets(
                &[&source, "let following = y; following;"],
                vm::CompileSourceFileOptions::default()
            ),
            vec![vm::Value::Float(2.5)],
        );
    }
}

#[test]
fn root_result_join_conservative_guards_preserve_optional_flags() {
    for (source, schema, optional) in [
        (
            "let y = if true => { null } else => { null };",
            Some(TypeSchema::Null),
            false,
        ),
        (
            "let y = match 1 { 1 => null, _ => null };",
            Some(TypeSchema::Null),
            false,
        ),
    ] {
        let compiled = vm::compile_source_for_repl_with_state_and_options(
            source,
            &[],
            vm::CompileSourceFileOptions::default(),
        )
        .unwrap();
        let y = compiled.bindings.iter().find(|b| b.name == "y").unwrap();
        println!("root result join: conservative source={source}, y={y:?}");
        assert_eq!(y.schema, schema, "{source}");
        assert_eq!(y.optional, optional, "{source}");
    }
    for expression in [
        "if flag => { left } else => { right }",
        "match choice { 1 => left, _ => right }",
    ] {
        let source = format!(
            "struct Left {{ value: int }} struct Right {{ value: int }} let flag = true; let choice = 1; let left: Left? = {{ value: 1 }}; let right: Right? = {{ value: 2 }}; let y = {expression};"
        );
        let error = vm::compile_source_for_repl_with_state_and_options(
            &source,
            &[],
            vm::CompileSourceFileOptions::default(),
        )
        .err()
        .expect("strict typing still rejects divergent nominal results");
        assert!(
            matches!(
                error,
                vm::SourceError::Compile(vm::CompileError::StrictTypingRequired { .. })
            ),
            "{error:?}"
        );
    }
}

#[test]
fn root_result_join_nullable_result_keeps_binding_effects_separate() {
    for expression in [
        "if flag => { x = 2.5; maybe } else => { x = 2.5; null }",
        "match choice { 1 => if flag => { x = 2.5; maybe } else => { x = 2.5; maybe }, _ => if flag => { x = 2.5; null } else => { x = 2.5; null } }",
    ] {
        let source = format!(
            "let flag = true; let choice = 1; let mut x = 1; let maybe: float? = 3.5; let y = {expression};"
        );
        let compiled = vm::compile_source_for_repl_with_state_and_options(
            &source,
            &[],
            vm::CompileSourceFileOptions::default(),
        )
        .unwrap();
        let x = compiled.bindings.iter().find(|b| b.name == "x").unwrap();
        let y = compiled.bindings.iter().find(|b| b.name == "y").unwrap();
        assert_eq!(x.schema, Some(TypeSchema::Float));
        assert!(!x.optional);
        assert_eq!(y.schema, Some(TypeSchema::Float));
        assert!(y.optional);
        #[cfg(feature = "runtime")]
        assert_eq!(
            execute_persistent_snippets(
                &[&source, "let following: float = x; following; y;"],
                vm::CompileSourceFileOptions::default()
            ),
            vec![vm::Value::Float(2.5), vm::Value::Float(3.5)],
        );
    }
}

fn sequential_callee_source() -> String {
    format!(
        "fn f() -> int {{ {} t256 }} f();",
        (0..257)
            .map(|i| format!("let t{i} = {i};"))
            .collect::<Vec<_>>()
            .join(" ")
    )
}

#[test]
fn persistence_allocator_compacts_257_sequential_callee_locals() {
    let source = sequential_callee_source();
    assert_eq!(source.len(), 3919);
    let compiled = vm::compile_source_for_repl_with_state(&source, &[])
        .expect("REPL must compact callee locals independently of persistent roots");
    assert!(compiled.bindings.is_empty());
    assert_eq!(
        compiled.compiled.program.callable_prototypes[0].frame_local_count,
        1
    );
    #[cfg(feature = "runtime")]
    {
        let mut machine = vm::Vm::new(compiled.compiled.program.clone());
        assert_eq!(machine.run().unwrap(), vm::VmStatus::Halted);
        assert_eq!(machine.stack(), &[vm::Value::Int(256)]);
    }
    #[cfg(all(feature = "cranelift-jit", target_arch = "x86_64"))]
    {
        let mut machine = vm::Vm::new(compiled.compiled.program);
        machine.compile_aot().unwrap();
        assert!(machine.has_aot_program());
        assert_eq!(machine.run().unwrap(), vm::VmStatus::Halted);
        assert_eq!(machine.stack(), &[vm::Value::Int(256)]);
        assert!(machine.aot_exec_count() > 0);
        println!(
            "capacity parity: interpreter and native AOT return Int(256), native executions={}",
            machine.aot_exec_count()
        );
    }
}

#[test]
fn persistence_allocator_ordinary_257_callee_control() {
    let compiled =
        vm::compile_source_with_flavor(&sequential_callee_source(), vm::SourceFlavor::RustScript)
            .unwrap();
    assert_eq!(compiled.program.callable_prototypes[0].frame_local_count, 1);
    #[cfg(feature = "runtime")]
    {
        let mut machine = vm::Vm::new(compiled.program);
        assert_eq!(machine.run().unwrap(), vm::VmStatus::Halted);
        assert_eq!(machine.stack(), &[vm::Value::Int(256)]);
    }
}

const NINE_MODULE_LOCALS: &str = "let a = 101; let b = 102; let c = 103; let d = 104; let e = 105; let f = 106; let g = 107; let h = 108; let i = 109; pub fn value() -> int { 1 }";

#[cfg(feature = "runtime")]
fn execute_persistent_snippets(
    snippets: &[&str],
    options: vm::CompileSourceFileOptions,
) -> Vec<vm::Value> {
    let mut states = Vec::<ReplLocalState>::new();
    let mut values = std::collections::BTreeMap::<String, vm::Value>::new();
    let mut results = Vec::new();
    for source in snippets {
        let compiled =
            vm::compile_source_for_repl_with_state_and_options(source, &states, options.clone())
                .unwrap();
        let mut machine = vm::Vm::new(compiled.compiled.program);
        for (name, value) in &values {
            let slot = machine.debug_info().unwrap().local_index(name).unwrap();
            machine.set_local(slot, value.clone()).unwrap();
        }
        assert_eq!(machine.run().unwrap(), vm::VmStatus::Halted);
        results.extend_from_slice(machine.stack());
        states.clear();
        values.clear();
        for binding in compiled.bindings {
            let slot = machine
                .debug_info()
                .unwrap()
                .local_index(&binding.name)
                .unwrap() as usize;
            values.insert(binding.name.clone(), machine.locals()[slot].clone());
            states.push(ReplLocalState {
                binding,
                moved: false,
            });
        }
    }
    results
}

#[cfg(feature = "runtime")]
#[test]
fn persistence_allocator_combines_roots_module_recursion_and_captures() {
    let large = format!(
        "use calibration; fn recurse(n: int) -> int {{ if n == 0 => {{ 0 }} else => {{ 1 + recurse(n - 1) }} }} fn large() -> int {{ let delta = base; let add = |value| value + delta; {} add(t256) + recurse(3) }} let fresh = 77; large();",
        (0..257)
            .map(|i| format!("let t{i} = {i};"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let results = execute_persistent_snippets(
        &[
            "let mut base = 41; let a = 11; let b = 12; let c = 13; let d = 14; let e = 15; let f = 16; let g = 17; let h = 18; let i = 19;",
            &large,
            "base = base + 1; base; a; b; c; d; e; f; g; h; i; fresh;",
        ],
        vm::CompileSourceFileOptions::default()
            .with_module_override_source("calibration.rss", NINE_MODULE_LOCALS),
    );
    let expected = std::iter::once(vm::Value::Int(256 + 41 + 3))
        .chain(std::iter::once(vm::Value::Int(42)))
        .chain((11..=19).map(vm::Value::Int))
        .chain(std::iter::once(vm::Value::Int(77)))
        .collect::<Vec<_>>();
    assert_eq!(results, expected);
}

#[test]
fn persistence_allocator_rejects_257_simultaneously_live_roots_and_callee() {
    let definitions = (0..257)
        .map(|i| format!("let t{i} = {i};"))
        .collect::<Vec<_>>()
        .join(" ");
    let uses = (0..257)
        .map(|i| format!("t{i}"))
        .collect::<Vec<_>>()
        .join(" + ");
    for source in [
        definitions.clone(),
        format!("fn f() -> int {{ {definitions} {uses} }} f();"),
    ] {
        let error = vm::compile_source_for_repl_with_state(&source, &[])
            .err()
            .expect("simultaneous liveness must still enforce the 256-slot limit");
        assert!(
            matches!(&error, vm::SourceError::Parse(error) if error.message == "too many simultaneously live locals (requires more than 256 slots)"),
            "{error:?}"
        );
    }
    let ordinary = vm::compile_source_with_flavor(
        &format!("fn f() -> int {{ {definitions} {uses} }} f();"),
        vm::SourceFlavor::RustScript,
    )
    .err()
    .expect("ordinary callee liveness limit is unchanged");
    assert!(
        matches!(&ordinary, vm::SourceError::Parse(error) if error.message == "too many simultaneously live locals (requires more than 256 slots)"),
        "{ordinary:?}"
    );
}

#[cfg(feature = "runtime")]
#[test]
fn final_layout_preserves_unused_entry_across_nine_local_module() {
    let options = vm::CompileSourceFileOptions::default()
        .with_module_override_source("calibration.rss", NINE_MODULE_LOCALS);
    let results = execute_persistent_snippets(
        &[
            "let x = 41;",
            "use calibration; calibration::value();",
            "x;",
        ],
        options,
    );
    assert_eq!(results.last(), Some(&vm::Value::Int(41)));
}

#[cfg(feature = "runtime")]
#[test]
fn final_layout_preserves_all_nine_first_snippet_locals() {
    let results = execute_persistent_snippets(
        &[
            "let a = 11; let b = 12; let c = 13; let d = 14; let e = 15; let f = 16; let g = 17; let h = 18; let i = 19;",
            "a;",
            "b;",
            "c;",
            "d;",
            "e;",
            "f;",
            "g;",
            "h;",
            "i;",
        ],
        vm::CompileSourceFileOptions::default(),
    );
    assert_eq!(results, (11..=19).map(vm::Value::Int).collect::<Vec<_>>());
}

#[test]
fn final_layout_ordinary_source_and_file_keep_compaction() {
    let source = "let a = 11; let b = 12; let c = 13; let d = 14; let e = 15; let f = 16; let g = 17; let h = 18; let i = 19; i;";
    let ordinary = vm::compile_source_with_flavor(source, vm::SourceFlavor::RustScript).unwrap();
    let file = vm::compile_source_at_path_with_flavor_and_options(
        "layout.rss",
        source,
        vm::SourceFlavor::RustScript,
        vm::CompileSourceFileOptions::default(),
    )
    .unwrap();
    assert!(ordinary.locals < 9, "{}", ordinary.locals);
    assert!(file.locals < 9, "{}", file.locals);
    let repl = vm::compile_source_for_repl_with_state(source, &[]).unwrap();
    assert!(repl.compiled.locals >= 9, "{}", repl.compiled.locals);
    #[cfg(feature = "runtime")]
    for program in [ordinary.program, file.program] {
        let mut machine = vm::Vm::new(program);
        machine.run().unwrap();
        assert_eq!(machine.stack().last(), Some(&vm::Value::Int(19)));
    }
}

#[test]
fn final_layout_named_optional_entry_seeds_follow_final_debug_slots() {
    let named = vm::HostStructSchema::new(
        "RootNamedOptional",
        vec![vm::HostStructField::new("value", vm::HostTypeSchema::Int)],
    );
    let mut builder = vm::HostApiBuilder::new();
    builder.named_struct(named.clone());
    builder.function(vm::HostFunctionSchema::with_return(
        "layout::root",
        vec![],
        named.as_type(),
    ));
    let options = vm::CompileSourceFileOptions::default()
        .with_host_api_catalog(std::sync::Arc::new(builder.build().unwrap()))
        .with_module_override_source("calibration.rss", NINE_MODULE_LOCALS);
    let first = vm::compile_source_for_repl_with_state_and_options(
        "let root = layout::root();",
        &[],
        options.clone(),
    )
    .unwrap();
    let slot = first
        .compiled
        .program
        .debug
        .as_ref()
        .unwrap()
        .local_index("root")
        .unwrap() as usize;
    let schema = first
        .compiled
        .program
        .type_map
        .as_ref()
        .unwrap()
        .local_schemas[slot]
        .clone()
        .unwrap();
    assert_eq!(
        schema,
        TypeSchema::Named("RootNamedOptional".into(), vec![])
    );
    let states = [false, true].map(|optional| ReplLocalState {
        binding: ReplLocalBinding {
            name: if optional { "maybe" } else { "root" }.into(),
            mutable: false,
            schema: Some(schema.clone()),
            optional,
        },
        moved: false,
    });
    let large = format!("use calibration; {}", sequential_callee_source());
    for source in [
        "use calibration; root.value; maybe == null; calibration::value();",
        "use calibration; calibration::value();",
        large.as_str(),
        "use calibration; fn recurse(n: int) -> int { if n == 0 => { 0 } else => { 1 + recurse(n - 1) } } fn capture() -> int { let value = root.value; let add = |n| n + value; add(recurse(3)) } capture();",
    ] {
        let compiled =
            vm::compile_source_for_repl_with_state_and_options(source, &states, options.clone())
                .unwrap();
        let program = compiled.compiled.program;
        for state in &states {
            let slot = program
                .debug
                .as_ref()
                .unwrap()
                .local_index(&state.binding.name)
                .unwrap() as usize;
            let binding = compiled
                .bindings
                .iter()
                .find(|binding| binding.name == state.binding.name)
                .unwrap();
            assert_eq!(
                binding.schema,
                Some(schema.clone()),
                "{} in {source}, final debug slot {slot}",
                state.binding.name
            );
            assert_eq!(
                binding.optional, state.binding.optional,
                "{} in {source}, final debug slot {slot}",
                state.binding.name
            );
            // No-callee-conflict controls retain the previous backend map.
            if !source.contains("fn ") {
                let map = program.type_map.as_ref().unwrap();
                assert_eq!(map.local_schemas[slot], Some(schema.clone()));
                assert_eq!(map.optional_slots[slot], state.binding.optional);
            }
        }
    }
}

#[test]
fn public_repl_state_api_preserves_moved_local_semantics() {
    let binding = ReplLocalBinding {
        name: "message".to_string(),
        mutable: false,
        schema: Some(TypeSchema::String),
        optional: false,
    };

    let available = ReplLocalState {
        binding: binding.clone(),
        moved: false,
    };
    compile_source_for_repl_with_state("message;", &[available])
        .expect("available local should compile");

    let moved = ReplLocalState {
        binding,
        moved: true,
    };
    assert!(
        compile_source_for_repl_with_state("message;", &[moved]).is_err(),
        "moved local must be rejected"
    );
}

#[test]
fn native_repl_module_admission_rejects_wrong_types_resource_copy_and_moved_entry() {
    let options = vm::CompileSourceFileOptions::default()
        .with_module_override_source("calibration.rss", NINE_MODULE_LOCALS);
    let state = |name: &str, schema, moved| ReplLocalState {
        binding: ReplLocalBinding {
            name: name.into(),
            mutable: false,
            schema: Some(schema),
            optional: false,
        },
        moved,
    };
    let wrong = vm::compile_source_for_repl_with_state_and_options(
        "use calibration; let result: bool = x + calibration::value();",
        &[state("x", TypeSchema::Int, false)],
        options.clone(),
    )
    .err()
    .expect("admission must reject before producing a Program");
    assert!(matches!(wrong, vm::SourceError::Compile(_)), "{wrong:?}");
    let resource = vm::ResourceTypeKey::new("repl.admission-resource").unwrap();
    let copy = vm::compile_source_for_repl_with_state_and_options(
        "use calibration; let duplicate = owned.copy(); calibration::value();",
        &[state("owned", TypeSchema::Resource(resource), false)],
        options.clone(),
    )
    .err()
    .expect("admission must reject before producing a Program");
    assert!(
        matches!(&copy, vm::SourceError::Parse(error) if error.code.as_deref() == Some("E_OWNERSHIP_COPY_RESOURCE")),
        "{copy:?}"
    );
    let moved = vm::compile_source_for_repl_with_state_and_options(
        "use calibration; message; calibration::value();",
        &[state("message", TypeSchema::String, true)],
        options,
    )
    .err()
    .expect("admission must reject before producing a Program");
    assert!(
        matches!(&moved, vm::SourceError::Parse(error) if error.code.as_deref() == Some("E_LOCAL_MOVED")),
        "{moved:?}"
    );
    // Compilation has no VM/initializer authority. No failed admission result
    // exposes a Program that an embedding can initialize or execute.
}

#[cfg(feature = "runtime")]
#[test]
fn repl_options_preserve_named_host_identity_and_runtime_values_between_snippets() {
    use std::sync::Arc;
    use vm::{
        CallOutcome, CompileSourceFileOptions, HostApiBuilder, HostFunctionRegistry,
        HostFunctionSchema, HostParamSchema, HostStructField, HostStructSchema, HostTypeSchema,
        Value, Vm, VmStatus,
    };
    let point = HostStructSchema::new(
        "Calibration",
        vec![HostStructField::new("value", HostTypeSchema::Int)],
    );
    let mut builder = HostApiBuilder::new();
    builder.named_struct(point.clone());
    builder.function(HostFunctionSchema::with_return(
        "calibration::origin",
        vec![],
        point.as_type(),
    ));
    builder.function(HostFunctionSchema::with_return(
        "calibration::read",
        vec![HostParamSchema::value("point", point.as_type())],
        HostTypeSchema::Int,
    ));
    let catalog = Arc::new(builder.build().unwrap());
    let options = CompileSourceFileOptions::default().with_host_api_catalog(Arc::clone(&catalog));
    fn origin(_: &mut Vm, _: &[Value]) -> vm::VmResult<CallOutcome> {
        Ok(CallOutcome::Return(vm::return_one(Value::map(vec![(
            Value::string("value"),
            Value::Int(42),
        )]))))
    }
    fn read(_: &mut Vm, args: &[Value]) -> vm::VmResult<CallOutcome> {
        let Value::Map(fields) = &args[0] else {
            return Err(vm::VmError::TypeMismatch("Calibration"));
        };
        Ok(CallOutcome::Return(vm::return_one(
            fields.get(&Value::string("value")).unwrap().clone(),
        )))
    }
    let mut registry = HostFunctionRegistry::new();
    for (name, handler) in [
        ("calibration::origin", origin as vm::StaticHostFunction),
        ("calibration::read", read),
    ] {
        registry
            .register_catalog_static(
                vm::catalog_import_schemas(&catalog, name).remove(0),
                handler,
            )
            .unwrap();
    }
    let first = vm::compile_source_for_repl_with_state_and_options(
        "let point = calibration::origin();",
        &[],
        options.clone(),
    )
    .unwrap();
    let mut first_vm = Vm::new(first.compiled.program);
    registry.bind_vm_cached(&mut first_vm).unwrap();
    assert_eq!(first_vm.run().unwrap(), VmStatus::Halted);
    let slot = first_vm.debug_info().unwrap().local_index("point").unwrap();
    let schema = first_vm.program().type_map.as_ref().unwrap().local_schemas[slot as usize]
        .clone()
        .unwrap();
    assert!(matches!(&schema, TypeSchema::Named(name, _) if name == "Calibration"));
    let state = ReplLocalState {
        binding: ReplLocalBinding {
            name: "point".into(),
            mutable: false,
            schema: Some(schema),
            optional: false,
        },
        moved: false,
    };
    let second = vm::compile_source_for_repl_with_state_and_options(
        "calibration::read(point);",
        &[state],
        options.clone(),
    )
    .unwrap();
    let exact = second
        .compiled
        .program
        .host_import_schemas()
        .iter()
        .flatten()
        .find(|s| s.name == "calibration::read")
        .unwrap();
    assert_eq!(
        *exact,
        vm::catalog_import_schemas(&catalog, "calibration::read").remove(0)
    );
    let mut second_vm = Vm::new(second.compiled.program);
    registry.bind_vm_cached(&mut second_vm).unwrap();
    let next_slot = second_vm
        .debug_info()
        .unwrap()
        .local_index("point")
        .unwrap();
    second_vm
        .set_local(next_slot, first_vm.locals()[slot as usize].clone())
        .unwrap();
    assert_eq!(second_vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(second_vm.stack().last(), Some(&Value::Int(42)));
    let model = vm::analyze_source_from_string_with_options(
        "calibration.rss",
        "let point = calibration::origin(); point.value;",
        options.clone(),
    )
    .unwrap();
    assert_eq!(model.catalog().as_ref(), catalog.as_ref());
    let hints = vm::collect_inferred_local_type_hints_with_options(
        "let point = calibration::origin(); point.value;",
        vm::SourceFlavor::RustScript,
        options,
    )
    .unwrap();
    assert!(
        hints.iter().any(
            |h| matches!(&h.schema, Some(TypeSchema::Named(name, _)) if name == "Calibration")
        )
    );
}
