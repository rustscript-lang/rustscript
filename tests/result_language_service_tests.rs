#![cfg(feature = "runtime")]

use vm::{CompileSourceFileOptions, SourceFlavor, SourcePathError};

fn assert_origin(source: &str, entry: &str, dependency: bool, expected_slice: &str) {
    let path = std::path::Path::new("/result-provenance/main.rss");
    let options = if dependency {
        CompileSourceFileOptions::new().with_module_override_source("lib.rss", source)
    } else {
        CompileSourceFileOptions::new()
    };
    let error = vm::compile_source_at_path_with_flavor_and_options(
        path,
        entry,
        SourceFlavor::RustScript,
        options,
    )
    .err()
    .expect("admitted Result typing error must fail compilation");
    let SourcePathError::SourceWithMap { error, sources } = error else {
        panic!("expected full source ownership: {error:?}");
    };
    let rendered = match &error {
        vm::SourceError::Compile(error) => vm::render_compile_error(&sources, error, true),
        other => panic!("expected compile diagnostic: {other:?}"),
    };
    let vm::SourceError::Compile(vm::CompileError::StrictTypingRequired {
        span: Some(span),
        detail,
        ..
    }) = error
    else {
        panic!("expected Result typing diagnostic: {error:?}");
    };
    let owner = sources
        .file(span.source_id)
        .expect("diagnostic source must exist");
    assert_eq!(owner.text, source);
    assert!(
        owner
            .name
            .ends_with(if dependency { "/lib.rss" } else { "/main.rss" })
    );
    if dependency {
        assert_ne!(
            span.source_id, 0,
            "dependency must retain its own source id"
        );
        assert_eq!(sources.file(0).unwrap().text, entry);
    }
    assert_eq!(&owner.text[span.lo..span.hi], expected_slice);
    assert!(rendered.contains(&owner.name), "{rendered}");
    assert!(rendered.contains(expected_slice), "{rendered}");
    let (line, col) = sources
        .line_col_for_offset(span.source_id, span.lo)
        .unwrap();
    let prefix = &source[..span.lo];
    assert_eq!(
        line,
        prefix.bytes().filter(|byte| *byte == b'\n').count() + 1
    );
    assert_eq!(col, prefix.rsplit('\n').next().unwrap().chars().count() + 1);
    println!(
        "RESULT_SOURCE_ORIGIN id={} name={} line={line} col={col} slice={expected_slice:?} detail={detail}",
        span.source_id, owner.name
    );
}

#[test]
fn result_diagnostic_source_map_owns_standalone_and_imported_try() {
    let source = "// 変換\r\nfn fail() -> Result<int, string> { Result::Err(\"bad\") }\r\npub fn wrong() -> Result<int, int> {\r\n    let number = fail()?;\r\n    Result::Ok(number)\r\n}\r\n";
    for _ in 0..2 {
        assert_origin(source, source, false, "?");
        assert_origin(source, "use self::lib as lib;\nlib::wrong();\n", true, "?");
    }
}

#[test]
fn result_diagnostic_source_map_owns_nested_function_and_closure_try() {
    for body in [
        "fn inner() -> Result<int, int> { Result::Ok(fail()?) } inner()",
        "let inner: fn() -> Result<int, int> = || Result::Ok(fail()?); inner()",
    ] {
        let source = format!(
            "// 変換\r\nfn fail() -> Result<int, string> {{ Result::Err(\"bad\") }}\r\npub fn wrong() -> Result<int, int> {{ {body} }}\r\n"
        );
        assert_origin(&source, &source, false, "?");
        assert_origin(&source, "use self::lib as lib;\nlib::wrong();\n", true, "?");
    }
}

#[test]
fn result_diagnostic_source_map_owns_constructor_and_match() {
    for (source, slice) in [
        (
            "// 変換\r\nlet value: Result<int, string> = Result::Ok(\"bad\");\r\n",
            "Result::Ok(\"bad\")",
        ),
        // The parser currently assigns ResultMatch the next-token span.
        // This regression verifies its exact remapping/ownership, not a new extent.
        (
            "// 変換\r\nlet value = match 1 { Result::Ok(v) => v, Result::Err(e) => 0 };\r\n",
            ";",
        ),
    ] {
        assert_origin(source, source, false, slice);
        assert_origin(source, "use self::lib as lib;\n", true, slice);
    }
}

fn execute(source: &str) -> Vec<vm::Value> {
    let compiled = vm::compile_source(source).expect("formatter control must type check");
    let mut machine = vm::Vm::new(compiled.program.with_local_count(compiled.locals));
    machine.set_fuel(100_000);
    assert_eq!(machine.run().unwrap(), vm::VmStatus::Halted);
    machine.stack().to_vec()
}

fn assert_format(source: &str) -> String {
    let before = execute(source);
    let once = vm::format_source(source).expect("format succeeds");
    let twice = vm::format_source(&once).expect("second format succeeds");
    assert_eq!(once, twice, "compact-source formatting must be idempotent");
    assert_eq!(
        execute(&once),
        before,
        "format must preserve observable execution"
    );
    println!("RESULT_FORMAT_EXAMPLE {once:?}");
    once
}

#[test]
fn compact_result_format_is_idempotent_and_preserves_execution() {
    for constructor in ["Result::Ok(7)", "Result::Err(\"bad\")"] {
        let source = format!(
            "fn relay(value:Result<int,string>)->Result<int,string>{{let n=value?;Result::Ok(n+1)}}let result=relay({constructor});let answer=match result{{Result::Ok(v)=>v,Result::Err(e)=>-1}};answer;"
        );
        let formatted = assert_format(&source);
        assert!(formatted.contains("value?"));
        assert!(formatted.contains("Result<int, string>"));
        assert!(formatted.contains("Result::Ok(n + 1)"));
    }
}

#[test]
fn compact_ordinary_call_block_comment_and_generic_format_controls() {
    for source in [
        "fn add(a:int,b:int)->int{a+b}let answer=add(1,2);answer;",
        "fn identity<T>(v:T)->T{v}let answer=identity::<int>(3);answer;",
        "fn answer()->string{/* keep block */let value=\"literal  bytes\\n変換\";value}/* next */answer();",
        "fn answer()->int{let value=3;// keep line\nvalue}answer();",
    ] {
        let formatted = assert_format(source);
        for literal in [
            "/* keep block */",
            "/* next */",
            "// keep line",
            "\"literal  bytes\\n変換\"",
        ] {
            if source.contains(literal) {
                assert!(
                    formatted.contains(literal),
                    "comment/literal bytes must survive"
                );
            }
        }
    }
}
