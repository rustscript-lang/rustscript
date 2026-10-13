#[path = "frontend_plugin_fixtures/diagnostic_provenance.rs"]
mod diagnostic_provenance;
#[path = "frontend_plugin_fixtures/graph_acceptance.rs"]
mod graph_acceptance;
#[path = "frontend_plugin_fixtures/graph_format.rs"]
mod graph_format;
#[path = "frontend_plugin_fixtures/linked_admission.rs"]
mod linked_admission;
#[path = "frontend_plugin_fixtures/producer_policy.rs"]
mod producer_policy;
#[path = "frontend_plugin_fixtures/rejection_policy.rs"]
mod rejection_policy;
#[path = "frontend_plugin_fixtures/resource_policy.rs"]
mod resource_policy;

use std::path::{Path, PathBuf};
use vm::VmResult;

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
    ExportFunctionWithImports,
    ExportTypeAlias,
    ImportTypeAlias,
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
        if matches!(
            self.format,
            Format::ExportFunction | Format::ExportTypeAlias
        ) {
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
                if matches!(self.format, Format::ImportTypeAlias) {
                    let binding = line
                        .split_once('"')
                        .and_then(|(_, rest)| rest.split_once('"'))
                        .map(|(_, rest)| rest.trim().trim_start_matches("type "))
                        .unwrap_or_default()
                        .trim_end_matches(';');
                    let (imported, local) =
                        binding.split_once(" as ").unwrap_or((binding, binding));
                    Ok(ModuleImport::value(
                        spec,
                        ImportClause::Named(vec![NamedImport::type_only(imported, local)]),
                        1,
                    ))
                } else {
                    let imported = line
                        .split_once(" as ")
                        .map(|(_, imported)| imported.trim().trim_end_matches(';'))
                        .unwrap_or("plus");
                    Ok(ModuleImport::value(
                        spec,
                        ImportClause::Named(vec![NamedImport::value(imported, imported)]),
                        1,
                    ))
                }
            })
            .collect()
    }

    fn strip_import_directives(&self, source: &str) -> String {
        match self.format {
            Format::ExportFunction => source.to_string(),
            Format::ExportFunctionWithImports | Format::ImportTypeAlias => source
                .lines()
                .filter(|line| !line.starts_with("use "))
                .collect::<Vec<_>>()
                .join("\n"),
            Format::ExportTypeAlias => source.to_string(),
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
            Format::ExportTypeAlias => {
                if source.trim() != "export type Box = int" {
                    return Err(ParseError::new("expected exported Box alias"));
                }
                // This type producer also owns executable code. A type-only edge
                // must discover its alias without executing this initializer or
                // allocating its otherwise callable guest function.
                "pub fn hidden() -> int { 99 } hidden();".to_string()
            }
            Format::ImportTypeAlias => {
                let imported_type = context.imports.iter().find_map(|import| match import {
                    vm::FrontendImportedDeclaration::Type {
                        local,
                        symbol,
                        schema: vm::TypeSchema::Int,
                        declaration: None,
                    } if local == "Item" && symbol.module.0 > 0 => Some(symbol),
                    _ => None,
                });
                if imported_type.is_none() {
                    return Err(ParseError::new(
                        "type-only import did not provide the transparent Item alias",
                    ));
                }
                let call = source
                    .lines()
                    .find(|line| !line.starts_with("use "))
                    .and_then(|line| line.trim().strip_prefix("call "))
                    .ok_or_else(|| ParseError::new("expected synthetic call statement"))?;
                format!("{call};")
            }
            Format::ExportFunction | Format::ExportFunctionWithImports => {
                if matches!(self.format, Format::ExportFunctionWithImports) {
                    let imported_plus = context.imports.iter().find_map(|import| match import {
                        vm::FrontendImportedDeclaration::Function {
                            local, declaration, ..
                        } if local == "plus" => Some(declaration),
                        _ => None,
                    });
                    let has_integer_signature = imported_plus.is_some_and(|declaration| {
                        declaration.arg_schemas == [Some(vm::TypeSchema::Int)]
                            && declaration.return_schema == Some(vm::TypeSchema::Int)
                    });
                    if !has_integer_signature {
                        return Err(ParseError::new(
                            "nested import did not provide plus(value: int) -> int",
                        ));
                    }
                }
                let source = source
                    .lines()
                    .filter(|line| !line.starts_with("use "))
                    .collect::<Vec<_>>()
                    .join("\n");
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

    fn module_type_exports(
        &self,
        source: &str,
        _context: &SourceFrontendContext<'_>,
        _ir: &FrontendIr,
    ) -> Result<Vec<vm::compiler::FrontendTypeExport>, ParseError> {
        if matches!(self.format, Format::ExportTypeAlias)
            && source.trim() == "export type Box = int"
        {
            return Ok(vec![vm::compiler::FrontendTypeExport {
                source_name: "Box".to_string(),
                schema: vm::TypeSchema::Int,
                declaration: None,
            }]);
        }
        Ok(Vec::new())
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
static FORMAT_TYPE_EXPORT: TypedFormat = TypedFormat {
    id: "synthetic-format-type-export",
    extensions: &["fmt-type"],
    format: Format::ExportTypeAlias,
};
static FORMAT_TYPE_IMPORT: TypedFormat = TypedFormat {
    id: "synthetic-format-type-import",
    extensions: &["fmt-type-user"],
    format: Format::ImportTypeAlias,
};
static FORMAT_WITH_IMPORTS: TypedFormat = TypedFormat {
    id: "synthetic-format-with-imports",
    extensions: &["fmt-imports"],
    format: Format::ExportFunctionWithImports,
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
fn native_catalog_call_keeps_exact_host_identity_through_modules() {
    assert_catalog_call_through_modules(false);
}

#[test]
fn catalog_bound_dependency_accepts_host_free_plugin_root() {
    assert_catalog_call_through_modules(true);
}

fn assert_catalog_call_through_modules(plugin_root: bool) {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use vm::host_api::{HostApiBuilder, HostFunctionSchema, HostParamSchema, HostTypeSchema};

    let root = temp_root("native-catalog-identity");
    let library = root.join("lib.rss");
    let entry = root.join(if plugin_root {
        "main.fmt-two"
    } else {
        "main.rss"
    });
    write_source(
        &library,
        "use probe;\npub fn plus(value: int) -> int { probe::add(value) }",
    );
    write_source(
        &entry,
        if plugin_root {
            "use \"./lib.rss\";\ncall plus(41)"
        } else {
            "use self::lib as lib;\nlib::plus(41);"
        },
    );
    let mut builder = HostApiBuilder::new();
    builder.function(HostFunctionSchema::with_return(
        "probe::add",
        vec![HostParamSchema::value("value", HostTypeSchema::Int)],
        HostTypeSchema::Int,
    ));
    let catalog = Arc::new(builder.build().expect("catalog should be valid"));
    let expected = vm::catalog_import_schemas(&catalog, "probe::add")[0].clone();
    let compiled = compile_source_file_with_options(
        &entry,
        CompileSourceFileOptions::new()
            .with_source_plugin(&FORMAT_TWO)
            .with_host_api_catalog(Arc::clone(&catalog)),
    )
    .expect("native catalog call should survive module resolution");
    let imports: Vec<_> = compiled
        .program
        .imports
        .iter()
        .enumerate()
        .filter(|(_, import)| import.name == "probe::add")
        .collect();
    assert_eq!(imports.len(), 1, "exact host import must be emitted once");
    assert_eq!(
        compiled.program.host_import_schemas()[imports[0].0].as_ref(),
        Some(&expected)
    );

    struct AddHost(Arc<AtomicUsize>);
    impl vm::HostFunction for AddHost {
        fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> vm::VmResult<vm::CallOutcome> {
            self.0.fetch_add(1, Ordering::SeqCst);
            assert_eq!(args, &[Value::Int(41)]);
            Ok(vm::CallOutcome::Return(vm::CallReturn::one(Value::Int(42))))
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = vm::HostFunctionRegistry::new();
    let counter = Arc::clone(&calls);
    registry
        .register_catalog(expected, move || -> Box<dyn vm::HostFunction> {
            Box::new(AddHost(Arc::clone(&counter)))
        })
        .expect("exact host registration should succeed");
    let mut vm = Vm::new(compiled.program);
    registry
        .bind_vm_cached(&mut vm)
        .expect("exact catalog should bind");
    assert_eq!(
        vm.run().expect("native module host call should execute"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(42)]);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    std::fs::remove_dir_all(&root).expect("fixture directory should be removed");
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

#[test]
fn nested_plugin_import_resolves_relative_to_its_own_source() {
    let root = temp_root("nested-relative-import");
    let nested = root.join("nested");
    std::fs::create_dir_all(&nested).expect("nested directory should be created");
    let entry = root.join("main.fmt-two");
    let relay = nested.join("relay.fmt-imports");
    let dependency = nested.join("leaf.fmt-one");
    let decoy = root.join("leaf.fmt-one");
    write_source(
        &dependency,
        r#"export fn plus(value: int) -> int = value + 1"#,
    );
    write_source(&decoy, r#"export fn plus(value: string) -> string = value"#);
    write_source(
        &relay,
        "use \"./leaf.fmt-one\" as plus;\nexport fn relay(value: int) -> int = plus(value)",
    );
    write_source(
        &entry,
        "use \"./nested/relay.fmt-imports\" as relay;\ncall relay(41)",
    );

    let options = CompileSourceFileOptions::new()
        .with_source_plugin(&FORMAT_ONE)
        .with_source_plugin(&FORMAT_TWO)
        .with_source_plugin(&FORMAT_WITH_IMPORTS);
    let compiled = compile_source_file_with_options(&entry, options)
        .expect("nested relative import must resolve against the importing source");
    let mut vm = Vm::new(compiled.program);
    assert_eq!(
        vm.run().expect("linked program should execute"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(42)]);

    std::fs::remove_dir_all(&root).expect("fixture directory should be removed");
}

#[test]
fn type_only_import_preserves_transparent_alias_without_runtime_imports() {
    let root = temp_root("type-only-alias");
    let type_module = root.join("box.fmt-type");
    let entry = root.join("main.fmt-type-user");
    write_source(&type_module, "export type Box = int");
    write_source(&entry, "use \"./box.fmt-type\" type Box as Item;\ncall 42");

    let options = CompileSourceFileOptions::new()
        .with_source_plugin(&FORMAT_TYPE_EXPORT)
        .with_source_plugin(&FORMAT_TYPE_IMPORT);
    let compiled = compile_source_file_with_options(&entry, options)
        .expect("type-only alias should be visible during source parsing");
    assert!(
        compiled.functions.is_empty(),
        "a type-only import creates no function imports"
    );
    let mut vm = Vm::new(compiled.program);
    assert_eq!(
        vm.run().expect("linked program should execute"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(42)]);

    std::fs::remove_dir_all(&root).expect("fixture directory should be removed");
}
