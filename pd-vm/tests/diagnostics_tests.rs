use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use vm::{
    ParseError, SourceError, SourceMap, SourcePathError, Span, Vm, compile_source,
    compile_source_file, render_source_error, render_vm_error,
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
