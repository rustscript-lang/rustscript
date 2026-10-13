use super::*;

fn options() -> CompileSourceFileOptions {
    CompileSourceFileOptions::new()
        .with_source_plugin(&graph_format::GRAPH_FORMAT)
        .with_source_plugin(&FORMAT_ONE)
}
fn reject_in_all_entries(entry: &Path, source: &str, expected: &str) {
    write_source(entry, source);
    let errors = [
        compile_source_file_with_options(entry, options())
            .err()
            .unwrap(),
        vm::compiler::analyze_source_file_with_options(entry, options())
            .err()
            .unwrap(),
        vm::collect_inferred_local_type_hints_at_path_with_options(
            entry,
            source,
            vm::SourceFlavor::Plugin(FrontendId::new("neutral-graph-format")),
            options(),
        )
        .err()
        .unwrap(),
    ];
    for error in &errors {
        assert!(error.to_string().contains(expected), "{error}");
    }
    assert_eq!(errors[0].to_string(), errors[1].to_string());
    assert_eq!(errors[0].to_string(), errors[2].to_string());
}
#[test]
fn private_missing_types_and_cross_frontend_cycles_reject_in_all_checked_entries() {
    let root = temp_root("graph-negative-matrix");
    let entry = root.join("main.graph");
    write_source(
        &root.join("lib.rss"),
        "struct Secret { value: int }\npub fn answer() -> int { 42 }\n",
    );
    reject_in_all_entries(&entry, "bind type Secret Item ./lib.rss\n42;\n", "Secret");
    write_source(&root.join("lib.graph"), "struct Visible { value: int }\n");
    reject_in_all_entries(
        &entry,
        "bind type Missing Item ./lib.graph\n42;\n",
        "Missing",
    );
    write_source(
        &root.join("lib.fmt-one"),
        "export fn answer(value: int) -> int = value",
    );
    reject_in_all_entries(
        &entry,
        "bind type answer Item ./lib.fmt-one\n42;\n",
        "answer",
    );
    // A plugin -> native -> plugin cycle is diagnosed before either initializer can run.
    write_source(
        &root.join("loop.rss"),
        "use self::main as main;\npub fn answer() -> int { 42 }\n",
    );
    write_source(&entry, "bind value answer answer ./loop.rss\nanswer();\n");
    let configured = || options().with_module_override_path("./main.rss", &entry);
    let error = compile_source_file_with_options(&entry, configured())
        .err()
        .unwrap();
    assert!(error.to_string().contains("cycle"), "{error}");
    assert!(vm::compiler::analyze_source_file_with_options(&entry, configured()).is_err());
    assert!(
        vm::collect_inferred_local_type_hints_at_path_with_options(
            &entry,
            &std::fs::read_to_string(&entry).unwrap(),
            vm::SourceFlavor::Plugin(FrontendId::new("neutral-graph-format")),
            configured()
        )
        .is_err()
    );
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn source_and_path_overrides_keep_cross_frontend_identity_and_runtime_value() {
    let root = temp_root("graph-overrides");
    let entry = root.join("main.graph");
    let real = root.join("real.fmt-one");
    write_source(&real, "export fn plus(value: int) -> int = value + 1");
    let source = "bind value plus plus ./virtual.fmt-one\nlet result = plus(41);\nresult;\n";
    write_source(&entry, source);
    for by_path in [false, true] {
        let configured = || {
            if by_path {
                options().with_module_override_path("./virtual.fmt-one", &real)
            } else {
                options().with_module_override_source(
                    "./virtual.fmt-one",
                    "export fn plus(value: int) -> int = value + 1",
                )
            }
        };
        let compiled = compile_source_file_with_options(&entry, configured()).unwrap();
        let mut machine = Vm::new(compiled.program);
        assert_eq!(machine.run().unwrap(), VmStatus::Halted);
        assert_eq!(machine.stack(), &[Value::Int(42)]);
        vm::compiler::analyze_source_file_with_options(&entry, configured()).unwrap();
        let hints = vm::collect_inferred_local_type_hints_at_path_with_options(
            &entry,
            source,
            vm::SourceFlavor::Plugin(FrontendId::new("neutral-graph-format")),
            configured(),
        )
        .unwrap();
        let result = hints.iter().find(|hint| hint.name == "result").unwrap();
        assert_eq!(result.schema, Some(vm::TypeSchema::Int));
        let span = result.span.unwrap();
        assert_eq!(&source[span.lo..span.hi], "result");
    }
    std::fs::remove_dir_all(root).unwrap();
}
