use std::path::{Path, PathBuf};

use vm::{
    CompileSourceFileOptions, FrontendId, FrontendIr, FrontendTypingPolicy, ImportClause,
    ModuleImport, NamedImport, ParseError, ParserDialect, SharedParserOptions,
    SourceFrontendContext, SourceFrontendDescriptor, SourcePlugin, Value, Vm, VmStatus,
    compile_source_file_with_options, parse_source_with_dialect,
};

struct SyntheticDialect;
impl ParserDialect for SyntheticDialect {}
static DIALECT: SyntheticDialect = SyntheticDialect;

struct TypedFormat {
    id: &'static str,
    extensions: &'static [&'static str],
    format: Format,
}

#[derive(Clone, Copy)]
enum Format {
    ExportFunction,
    CallFunction,
}

impl SourcePlugin for TypedFormat {
    fn descriptor(&self) -> SourceFrontendDescriptor {
        SourceFrontendDescriptor {
            id: FrontendId::new(self.id),
            extensions: self.extensions,
            typing_policy: FrontendTypingPolicy::Strict,
            supports_ownership_checking: false,
        }
    }

    fn parse_module_imports(
        &self,
        source: &str,
        _path: &Path,
        _context: &SourceFrontendContext<'_>,
    ) -> Result<Vec<ModuleImport>, vm::SourcePathError> {
        if matches!(self.format, Format::ExportFunction) {
            return Ok(Vec::new());
        }
        source
            .lines()
            .filter(|line| line.starts_with("use "))
            .map(|line| {
                let spec = line
                    .strip_prefix("use \"")
                    .and_then(|line| line.split_once('\"'))
                    .map(|(spec, _)| spec.to_string())
                    .ok_or_else(|| vm::SourcePathError::UnsupportedExtension(line.to_string()))?;
                Ok(ModuleImport {
                    spec,
                    clause: ImportClause::Named(vec![NamedImport {
                        imported: "plus".to_string(),
                        local: "plus".to_string(),
                    }]),
                    line: 1,
                })
            })
            .collect()
    }

    fn strip_import_directives(&self, source: &str) -> String {
        match self.format {
            Format::ExportFunction => source.to_string(),
            Format::CallFunction => source
                .lines()
                .filter(|line| !line.starts_with("use "))
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    fn parse_source(
        &self,
        source: &str,
        context: &SourceFrontendContext<'_>,
    ) -> Result<FrontendIr, ParseError> {
        let rss = match self.format {
            Format::ExportFunction => {
                let declaration = source.strip_prefix("export ").ok_or_else(|| {
                    ParseError::new("expected synthetic exported-function declaration")
                })?;
                let (signature, expr) = declaration.split_once(" = ").ok_or_else(|| {
                    ParseError::new("expected '=' in synthetic exported-function declaration")
                })?;
                let mut rss = signature.replace("fn ", "pub fn ");
                rss.push_str(" { ");
                rss.push_str(expr);
                rss.push_str(" }");
                rss
            }
            Format::CallFunction => {
                let call = source
                    .trim()
                    .strip_prefix("call ")
                    .ok_or_else(|| ParseError::new("expected synthetic call statement"))?;
                format!("{call};")
            }
        };
        parse_source_with_dialect(
            &rss,
            &DIALECT,
            SharedParserOptions {
                source_id: context.source_id,
                allow_implicit_externs: true,
                allow_implicit_semicolons: false,
                enforce_mutable_bindings: true,
                import_scan_mode: false,
            },
        )
    }
}

static FORMAT_ONE: TypedFormat = TypedFormat {
    id: "synthetic-format-one",
    extensions: &["fmt-one"],
    format: Format::ExportFunction,
};
static FORMAT_TWO: TypedFormat = TypedFormat {
    id: "synthetic-format-two",
    extensions: &["fmt-two"],
    format: Format::CallFunction,
};

fn temp_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "pd-vm-plugin-module-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).expect("fixture directory should be created");
    root
}

fn write_source(path: &Path, source: &str) {
    std::fs::write(path, source).expect("fixture source should be written");
}

#[test]
fn plugin_source_imports_and_runs_typed_plugin_dependency() {
    let root = temp_root("plugin-to-plugin");
    let library = root.join("lib.fmt-one");
    let entry = root.join("main.fmt-two");
    write_source(&library, r#"export fn plus(value: int) -> int = value + 1"#);
    write_source(
        &entry,
        r#"use "./lib.fmt-one";
call plus(41)"#,
    );

    let options = CompileSourceFileOptions::new()
        .with_source_plugin(&FORMAT_ONE)
        .with_source_plugin(&FORMAT_TWO);
    let compiled = compile_source_file_with_options(&entry, options)
        .expect("registered plugin dependency should compile");
    let mut vm = Vm::new(compiled.program);
    assert_eq!(
        vm.run().expect("linked program should execute"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(42)]);

    std::fs::remove_dir_all(&root).expect("fixture directory should be removed");
}

#[test]
fn plugin_source_imports_and_runs_native_rustscript_dependency() {
    let root = temp_root("plugin-to-rss");
    let library = root.join("lib.rss");
    let entry = root.join("main.fmt-two");
    write_source(&library, r#"pub fn plus(value: int) -> int { value + 1 }"#);
    write_source(
        &entry,
        r#"use "./lib.rss";
call plus(41)"#,
    );

    let options = CompileSourceFileOptions::new().with_source_plugin(&FORMAT_TWO);
    let compiled = compile_source_file_with_options(&entry, options)
        .expect("plugin importer should call a native RSS dependency");
    let mut vm = Vm::new(compiled.program);
    assert_eq!(
        vm.run().expect("linked program should execute"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(42)]);

    std::fs::remove_dir_all(&root).expect("fixture directory should be removed");
}

#[test]
fn plugin_imported_signature_rejects_incompatible_argument() {
    let root = temp_root("plugin-argument-schema");
    let library = root.join("lib.fmt-one");
    let entry = root.join("main.fmt-two");
    write_source(&library, r#"export fn plus(value: int) -> int = value + 1"#);
    write_source(
        &entry,
        r#"use "./lib.fmt-one";
call plus("wrong")"#,
    );

    let options = CompileSourceFileOptions::new()
        .with_source_plugin(&FORMAT_ONE)
        .with_source_plugin(&FORMAT_TWO);
    let error = match compile_source_file_with_options(&entry, options) {
        Ok(_) => panic!("imported integer parameter schema must reject a string argument"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("string"),
        "unexpected error: {error}"
    );

    std::fs::remove_dir_all(&root).expect("fixture directory should be removed");
}
