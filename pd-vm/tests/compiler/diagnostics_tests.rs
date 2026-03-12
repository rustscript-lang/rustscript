use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use vm::{
    ParseError, SourceError, SourceFlavor, SourceMap, SourcePathError, Span, Vm,
    collect_inferred_local_type_hints, compile_source, compile_source_file,
    lint_unknown_inferred_local_types, render_compile_error, render_source_error, render_vm_error,
};

#[test]
fn render_source_error_highlights_exact_range() {
    let source = "let value = 123;\nlet x = ;\n";
    let mut source_map = SourceMap::new();
    let source_id = source_map.add_source("inline.rss", source);
    let lo = source_map
        .line_col_to_offset(source_id, 2, 9)
        .expect("line/col offset should exist");
    let hi = lo + 1;

    let err = ParseError {
        line: 2,
        message: "expected expression".to_string(),
        span: Some(Span::new(source_id, lo, hi)),
        code: Some("E_PARSE".to_string()),
    };

    let rendered = render_source_error(&source_map, &err, false);
    assert!(rendered.contains("expected expression"));
    assert!(rendered.contains("inline.rss:2:9"));
    assert!(rendered.contains("2 | let x = ;"));
    assert!(rendered.contains("^"));
}

#[test]
fn compile_source_file_parse_error_uses_original_line() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be monotonic")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("pd_vm_diag_{unique}.js"));
    let source = "const broken = ;\n";
    fs::write(&path, source).expect("temp source should be writable");

    let result = compile_source_file(&path);
    let _ = fs::remove_file(&path);

    match result {
        Err(SourcePathError::Source(SourceError::Parse(parse))) => {
            assert_eq!(parse.line, 1);
            assert!(parse.span.is_some());

            let mut source_map = SourceMap::new();
            let source_id = source_map.add_source(path.display().to_string(), source.to_string());
            let parse = parse.with_line_span_from_source(&source_map, source_id);
            let rendered = render_source_error(&source_map, &parse, false);
            assert!(rendered.contains(":1:"));
            assert!(rendered.contains("const broken = ;"));
        }
        _ => panic!("expected parse error"),
    }
}

#[test]
fn render_compile_error_highlights_if_else_mismatch_line() {
    let source =
        "let cond = 1 == 1;\nlet value = if cond => {\n    1\n} else => {\n    \"x\"\n};\n";

    let err = match compile_source(source) {
        Ok(_) => panic!("compile should reject if/else mismatch"),
        Err(err) => err,
    };
    let compile = match err {
        SourceError::Compile(compile) => compile,
        other => panic!("expected compile error, got {other:?}"),
    };

    let line = compile
        .line()
        .expect("if/else mismatch should report a line");
    let mut source_map = SourceMap::new();
    let source_id = source_map.add_source("inline.rss", source);
    let line_text = source_map
        .file(source_id)
        .and_then(|file| file.line_text(line))
        .expect("line text should exist");
    let rendered = render_compile_error(&source_map, &compile, false);

    assert!(rendered.contains("if/else branches produced incompatible expression result"));
    assert!(rendered.contains("int vs string"));
    assert!(rendered.contains(&format!("inline.rss:{line}:1")));
    assert!(rendered.contains(line_text));
}

#[test]
fn compile_error_from_imported_module_reports_module_path() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be monotonic")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("pd_vm_diag_import_{unique}"));
    fs::create_dir_all(&root).expect("temp module root should be writable");

    let module_path = root.join("module.rss");
    let module_source = r#"let cond = 1 == 1;
let broken = if cond => {
    1
} else => {
    "x"
};

pub fn ok() {
    0;
}
"#;
    fs::write(&module_path, module_source).expect("module source should be writable");

    let main_path = root.join("main.rss");
    fs::write(&main_path, "use module;\nok();\n").expect("main source should be writable");

    let result = compile_source_file(&main_path);

    let _ = fs::remove_file(&main_path);
    let _ = fs::remove_file(&module_path);
    let _ = fs::remove_dir(&root);

    match result {
        Err(SourcePathError::Source(SourceError::Compile(compile))) => {
            assert_eq!(
                compile.source_name(),
                Some(module_path.to_string_lossy().as_ref())
            );
            assert_eq!(compile.line(), Some(2));

            let mut source_map = SourceMap::new();
            source_map.add_source(module_path.display().to_string(), module_source);
            let rendered = render_compile_error(&source_map, &compile, false);
            assert!(rendered.contains(&format!("{}:2:1", module_path.display())));
            assert!(rendered.contains("let broken = if cond => {"));
            assert!(rendered.contains("int vs string"));
        }
        _ => panic!("expected compile error"),
    }
}

#[test]
fn render_vm_error_includes_ip_and_source_line() {
    let source = "let value = 1 / 0;\n";
    let compiled = compile_source(source).expect("source should compile");
    let mut vm = Vm::new(compiled.program);
    let err = vm
        .run()
        .expect_err("runtime should fail with division by zero");

    let rendered = render_vm_error(&vm, &err);
    assert!(rendered.contains("runtime error"));
    assert!(rendered.contains("at ip"));
    assert!(rendered.contains("line 1"));
    assert!(rendered.contains("let value = 1 / 0;"));
}

#[test]
fn render_compile_error_highlights_invalid_schema_field_access_line() {
    let source = "struct User { name: string }\nlet user: User = { name: \"Ada\" };\nuser.age;\n";

    let err = match compile_source(source) {
        Ok(_) => panic!("compile should reject invalid schema field access"),
        Err(err) => err,
    };
    let compile = match err {
        SourceError::Compile(compile) => compile,
        other => panic!("expected compile error, got {other:?}"),
    };

    let line = compile
        .line()
        .expect("schema field access should report a line");
    let mut source_map = SourceMap::new();
    let source_id = source_map.add_source("inline.rss", source);
    let line_text = source_map
        .file(source_id)
        .and_then(|file| file.line_text(line))
        .expect("line text should exist");
    let rendered = render_compile_error(&source_map, &compile, false);

    assert!(rendered.contains("field 'age' is not declared"));
    assert!(rendered.contains(&format!("inline.rss:{line}:1")));
    assert!(rendered.contains(line_text));
}

#[test]
fn render_compile_error_highlights_captured_schema_field_access_inside_named_function() {
    let source = r#"struct User { name: string }
let user: User = { name: "Ada" };
fn show_age() {
    user.age;
}
show_age();
"#;

    let err = match compile_source(source) {
        Ok(_) => panic!("compile should reject invalid captured schema field access"),
        Err(err) => err,
    };
    let compile = match err {
        SourceError::Compile(compile) => compile,
        other => panic!("expected compile error, got {other:?}"),
    };

    let line = compile
        .line()
        .expect("captured schema field access should report a line");
    let mut source_map = SourceMap::new();
    let source_id = source_map.add_source("inline.rss", source);
    let line_text = source_map
        .file(source_id)
        .and_then(|file| file.line_text(line))
        .expect("line text should exist");
    let rendered = render_compile_error(&source_map, &compile, false);

    assert!(rendered.contains("field 'age' is not declared"));
    assert!(rendered.contains(&format!("inline.rss:{line}:1")));
    assert!(rendered.contains(line_text));
}

#[test]
fn lint_unknown_inferred_local_types_includes_function_scope_locals() {
    let source = r#"
fn nothing() {
    let a = [1, "2"];
    let idx = 0;
    let b = a[idx];
    0
}
"#;

    let warnings = lint_unknown_inferred_local_types(source, SourceFlavor::RustScript)
        .expect("lint should succeed");
    assert!(
        warnings
            .iter()
            .any(|warning| warning.name == "b" && warning.line == 5),
        "expected function-local unknown inferred warning for 'b', got {warnings:?}"
    );
}

#[test]
fn lint_unknown_inferred_local_types_respects_declared_schema_annotations() {
    let source = r#"
use json;
struct Stats { score: int }
struct Profile { stats: Stats }

let payload_json = json::encode({});
let payload_decoded: Profile = json::decode(payload_json);
payload_decoded.stats.score;
"#;

    let warnings = lint_unknown_inferred_local_types(source, SourceFlavor::RustScript)
        .expect("lint should succeed");
    assert!(
        warnings
            .iter()
            .all(|warning| warning.name != "payload_decoded"),
        "declared schema binding should not be reported as unknown, got {warnings:?}"
    );
}

#[test]
fn collect_inferred_local_type_hints_reports_visible_local_types() {
    let source = r#"
let total = 1;
print(total);
"#;

    let hints = collect_inferred_local_type_hints(source, SourceFlavor::RustScript)
        .expect("type hints should succeed");
    let total = hints
        .iter()
        .find(|hint| hint.name == "total")
        .expect("expected a type hint for total");

    assert_eq!(total.inferred_type, "int");
    assert_eq!(total.declared_line, Some(2));
    assert_eq!(total.last_line, Some(3));
}

#[test]
fn collect_inferred_local_type_hints_include_named_function_parameters() {
    let source = r#"
fn plus_one(amount) {
    amount + 1
}

plus_one(2);
"#;

    let hints = collect_inferred_local_type_hints(source, SourceFlavor::RustScript)
        .expect("type hints should succeed");
    let amount = hints
        .iter()
        .find(|hint| hint.name == "amount")
        .expect("expected a type hint for amount");

    assert_eq!(amount.inferred_type, "int");
    assert_eq!(amount.declared_line, Some(2));
}

#[test]
fn collect_inferred_local_type_hints_keep_function_body_expr_visibility() {
    let source = r#"
fn plus_one(amount) {
    let total = amount + 1;
    total
}

plus_one(2);
"#;

    let hints = collect_inferred_local_type_hints(source, SourceFlavor::RustScript)
        .expect("type hints should succeed");
    let total = hints
        .iter()
        .find(|hint| hint.name == "total")
        .expect("expected a type hint for total");

    assert_eq!(total.inferred_type, "int");
    assert_eq!(total.declared_line, Some(3));
    assert_eq!(total.last_line, Some(4));
}

#[test]
fn collect_inferred_local_type_hints_use_declared_schema_bound_type() {
    let source = r#"
use json;
struct Stats { score: int }
struct Profile { stats: Stats }

let payload_json = json::encode({});
let payload_decoded: Profile = json::decode(payload_json);
payload_decoded.stats.score;
"#;

    let hints = collect_inferred_local_type_hints(source, SourceFlavor::RustScript)
        .expect("type hints should succeed");
    let payload_decoded = hints
        .iter()
        .find(|hint| hint.name == "payload_decoded")
        .expect("expected a type hint for payload_decoded");

    assert_eq!(payload_decoded.inferred_type, "map");
    assert_eq!(payload_decoded.declared_line, Some(7));
    assert_eq!(payload_decoded.last_line, Some(8));
}
