//! Declared record omission is a schema contract shared by native and plugin sources.

use vm::{
    CompileSourceFileOptions, FrontendId, FrontendIr, FrontendTypingPolicy, ParseError,
    ParserDialect, SharedParserOptions, SourceFlavor, SourceFrontendContext,
    SourceFrontendDescriptor, SourcePlugin, TypeSchema, Value, Vm, VmStatus,
    analyze_source_from_string_with_options, collect_inferred_local_type_hints_with_options,
    compile_source_with_flavor_and_options, parse_source_with_dialect,
};

struct RecordDialect;
impl ParserDialect for RecordDialect {}

struct RecordPlugin {
    id: &'static str,
    extensions: &'static [&'static str],
}
impl SourcePlugin for RecordPlugin {
    fn descriptor(&self) -> SourceFrontendDescriptor {
        SourceFrontendDescriptor {
            id: FrontendId::new(self.id),
            extensions: self.extensions,
            typing_policy: FrontendTypingPolicy::Strict,
            supports_ownership_checking: true,
        }
    }

    fn parse_source(
        &self,
        source: &str,
        context: &SourceFrontendContext<'_>,
    ) -> Result<FrontendIr, ParseError> {
        parse_source_with_dialect(
            source,
            &RecordDialect,
            SharedParserOptions {
                source_id: context.source_id,
                import_scan_mode: true,
                ..SharedParserOptions::default()
            },
        )
    }
}

static RECORD_A: RecordPlugin = RecordPlugin {
    id: "opaque-record-a",
    extensions: &["record-a"],
};
static RECORD_B: RecordPlugin = RecordPlugin {
    id: "opaque-record-b",
    extensions: &["record-b"],
};

fn options(plugin: Option<&'static RecordPlugin>) -> CompileSourceFileOptions {
    let options = CompileSourceFileOptions::new();
    match plugin {
        Some(plugin) => options.with_source_plugin(plugin),
        None => options,
    }
}

fn flavor(plugin: Option<&'static RecordPlugin>) -> SourceFlavor {
    match plugin {
        Some(plugin) => SourceFlavor::Plugin(FrontendId::new(plugin.id)),
        None => SourceFlavor::RustScript,
    }
}

fn source_path(plugin: Option<&'static RecordPlugin>) -> String {
    format!(
        "main.{}",
        plugin.map_or("rss", |plugin| plugin.extensions[0])
    )
}

const OMITTED: &str =
    "struct Entry { count: int, note: string? }\nlet entry: Entry = { count: 42 };\nentry.count;\n";

fn assert_checked_run(source: &str, plugin: Option<&'static RecordPlugin>) {
    let compiled = compile_source_with_flavor_and_options(source, flavor(plugin), options(plugin))
        .unwrap_or_else(|error| panic!("checked compilation: {error}"));
    let mut machine = Vm::new(compiled.program);
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    assert_eq!(machine.stack(), &[Value::Int(42)]);

    let model =
        analyze_source_from_string_with_options(source_path(plugin), source, options(plugin))
            .unwrap();
    assert!(model.diagnostics().is_empty(), "{:?}", model.diagnostics());
    let hints =
        collect_inferred_local_type_hints_with_options(source, flavor(plugin), options(plugin))
            .unwrap();
    let hint = hints.iter().find(|hint| hint.name == "entry").unwrap();
    assert_eq!(
        model.ir().source_schema(hint.schema.as_ref().unwrap(), 0),
        TypeSchema::Named("Entry".to_string(), vec![])
    );
    let span = hint.span.unwrap();
    assert_eq!(&source[span.lo..span.hi], "entry");
    assert_eq!(
        model.inferred_schema_at(vm::SourcePosition::new(0, span.lo)),
        hint.schema
    );
}

#[test]
fn native_omitted_optional_record_field_compiles_analyzes_hints_and_runs() {
    assert_checked_run(OMITTED, None);
}

#[test]
fn opaque_a_omitted_optional_record_field_compiles_analyzes_hints_and_runs() {
    assert_checked_run(OMITTED, Some(&RECORD_A));
}

#[test]
fn opaque_b_omitted_optional_record_field_compiles_analyzes_hints_and_runs() {
    assert_checked_run(OMITTED, Some(&RECORD_B));
}

fn all_frontends() -> [Option<&'static RecordPlugin>; 3] {
    [None, Some(&RECORD_A), Some(&RECORD_B)]
}

#[test]
fn present_valid_optional_record_field_runs_across_frontends() {
    let source = OMITTED.replace("{ count: 42 }", "{ count: 42, note: \"ready\" }");
    for plugin in all_frontends() {
        assert_checked_run(&source, plugin);
    }
}

#[test]
fn present_null_optional_record_field_runs_across_frontends() {
    let source = OMITTED.replace("{ count: 42 }", "{ count: 42, note: null }");
    for plugin in all_frontends() {
        assert_checked_run(&source, plugin);
    }
}

#[test]
fn nested_named_generic_optional_fields_run_across_frontends() {
    let source = "struct Parcel<T> { value: T, extra: T? }\nstruct Entry { parcel: Parcel<int>, note: string? }\nlet entry: Entry = { parcel: { value: 42 } };\nentry.parcel.value;\n";
    for plugin in all_frontends() {
        assert_checked_run(source, plugin);
    }
}

#[test]
fn optional_record_omission_in_function_return_and_argument_runs_across_frontends() {
    let source = "struct Entry { count: int, note: string? }\nfn make() -> Entry { { count: 42 } }\nfn read(value: Entry) -> int { value.count }\nlet entry: Entry = make();\nread({ count: entry.count });\n";
    for plugin in all_frontends() {
        assert_checked_run(source, plugin);
    }
}

fn assert_rejected(source: &str, detail: &str, plugin: Option<&'static RecordPlugin>) {
    let compile = compile_source_with_flavor_and_options(source, flavor(plugin), options(plugin))
        .err()
        .expect("checked compilation must reject the schema");
    let analyze =
        analyze_source_from_string_with_options(source_path(plugin), source, options(plugin))
            .err()
            .expect("analysis must reject the schema");
    let hints =
        collect_inferred_local_type_hints_with_options(source, flavor(plugin), options(plugin))
            .expect_err("hints must reject the schema");
    for error in [compile, analyze, hints] {
        assert!(error.to_string().contains(detail), "{error}");
    }
}

#[test]
fn omitted_required_record_field_remains_rejected_across_frontends() {
    let source = OMITTED.replace("{ count: 42 }", "{ note: null }");
    for plugin in all_frontends() {
        assert_rejected(
            &source,
            "field 'count' is required by the declared schema but is missing",
            plugin,
        );
    }
}

#[test]
fn wrong_present_optional_field_remains_rejected_across_frontends() {
    let source = OMITTED.replace("{ count: 42 }", "{ count: 42, note: 7 }");
    for plugin in all_frontends() {
        assert_rejected(
            &source,
            "field 'note' is declared as schema type 'string' but was assigned int",
            plugin,
        );
    }
}

#[test]
fn nested_required_generic_field_remains_rejected_across_frontends() {
    let source = "struct Parcel<T> { value: T, extra: T? }\nstruct Entry { parcel: Parcel<int>, note: string? }\nlet entry: Entry = { parcel: { extra: 7 } };\nentry.parcel.value;\n";
    for plugin in all_frontends() {
        assert_rejected(
            source,
            "field 'parcel.value' is required by the declared schema but is missing",
            plugin,
        );
    }
}

#[test]
fn nested_wrong_present_generic_optional_field_remains_rejected_across_frontends() {
    let source = "struct Parcel<T> { value: T, extra: T? }\nstruct Entry { parcel: Parcel<int>, note: string? }\nlet entry: Entry = { parcel: { value: 42, extra: \"wrong\" } };\nentry.parcel.value;\n";
    for plugin in all_frontends() {
        assert_rejected(
            source,
            "field 'parcel.extra' is declared as schema type 'int' but was assigned string",
            plugin,
        );
    }
}

#[test]
fn omitted_required_resource_field_remains_rejected_across_frontends() {
    let source = "struct Entry { handle: resource<neutral.handle>, note: string? }\nlet entry: Entry = { note: null };\n42;\n";
    for plugin in all_frontends() {
        assert_rejected(
            source,
            "field 'handle' is required by the declared schema but is missing",
            plugin,
        );
    }
}

#[test]
fn independent_strict_unknown_rejection_survives_optional_omission() {
    let source = format!("fn identity(value: unknown) {{ 7 }}\n{OMITTED}");
    for plugin in [&RECORD_A, &RECORD_B] {
        assert_rejected(
            &source,
            "'unknown' annotations are not allowed",
            Some(plugin),
        );
    }
}

#[test]
fn optional_field_does_not_make_callable_argument_optional() {
    let source = "struct Entry { count: int, note: string? }\nfn read(value: Entry) -> int { value.count }\nread();\n";
    for plugin in all_frontends() {
        assert_rejected(source, "argument", plugin);
    }
}
