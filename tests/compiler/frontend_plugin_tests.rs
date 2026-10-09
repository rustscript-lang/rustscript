use std::fs::OpenOptions;
use std::io::Write;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use vm::{
    CompileSourceFileOptions, Expr, FrontendId, FrontendIr, FrontendTypingPolicy, ParseError,
    ParsePhase, SourceFlavor, SourceFrontendContext, SourceFrontendDescriptor, SourcePlugin, Stmt,
    Value, Vm, VmStatus, compile_source_file_with_options, compile_source_with_flavor_and_options,
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct SeenContext {
    call: &'static str,
    source_id: u32,
    original_path: Option<PathBuf>,
    phase: ParsePhase,
    catalog_ptr: Option<usize>,
    import_count: usize,
}

static PLUG_A_CONTEXTS: Mutex<Vec<SeenContext>> = Mutex::new(Vec::new());
static PLUG_B_CONTEXTS: Mutex<Vec<SeenContext>> = Mutex::new(Vec::new());
static PLUG_C_CONTEXTS: Mutex<Vec<SeenContext>> = Mutex::new(Vec::new());
static PLUG_D_CONTEXTS: Mutex<Vec<SeenContext>> = Mutex::new(Vec::new());

struct SyntheticPlugin {
    id: &'static str,
    extensions: &'static [&'static str],
    typing_policy: FrontendTypingPolicy,
    value: i64,
    reject_unknown: bool,
    contexts: &'static Mutex<Vec<SeenContext>>,
}

impl SyntheticPlugin {
    fn record(&self, call: &'static str, context: &SourceFrontendContext<'_>) {
        self.contexts.lock().unwrap().push(SeenContext {
            call,
            source_id: context.source_id,
            original_path: context.original_path.map(Path::to_path_buf),
            phase: context.phase,
            catalog_ptr: context
                .host_api_catalog
                .map(|catalog| catalog as *const _ as usize),
            import_count: context.imports.len(),
        });
    }

    fn ir(&self) -> FrontendIr {
        let struct_schemas = if self.reject_unknown {
            std::iter::once((
                "SyntheticRecord".to_string(),
                vm::compiler::ir::StructDecl {
                    name: "SyntheticRecord".to_string(),
                    type_params: Vec::new(),
                    body_schema: vm::TypeSchema::Object(
                        std::iter::once(("value".to_string(), vm::TypeSchema::Unknown)).collect(),
                    ),
                    origin: vm::compiler::StructDeclOrigin::Guest,
                },
            ))
            .collect()
        } else {
            Default::default()
        };
        FrontendIr {
            stmts: vec![Stmt::Expr {
                expr: Expr::Int(self.value),
                line: 1,
            }],
            locals: 0,
            local_bindings: Vec::new(),
            struct_schemas,
            unknown_type_spans: Vec::new(),
            functions: Vec::new(),
            function_impls: Default::default(),
            stmt_sources: Vec::new(),
            function_sources: Default::default(),
            use_declarations: Vec::new(),
            implicit_extern_names: Vec::new(),
            host_api_metadata: None,
            semantic_index: None,
            parsed_semantic_index: None,
            catalog_visibility: None,
            lexer_tokens: Vec::new(),
        }
    }
}

impl SourcePlugin for SyntheticPlugin {
    fn descriptor(&self) -> SourceFrontendDescriptor {
        SourceFrontendDescriptor {
            id: FrontendId::new(self.id),
            extensions: self.extensions,
            typing_policy: self.typing_policy,
            supports_ownership_checking: false,
        }
    }

    fn parse_source(
        &self,
        _source: &str,
        context: &SourceFrontendContext<'_>,
    ) -> Result<FrontendIr, ParseError> {
        self.record("parse", context);
        Ok(self.ir())
    }

    fn parse_module_imports(
        &self,
        _source: &str,
        _path: &Path,
        context: &SourceFrontendContext<'_>,
    ) -> Result<Vec<vm::ModuleImport>, vm::SourcePathError> {
        self.record("imports", context);
        Ok(Vec::new())
    }
}

static PLUG_A: SyntheticPlugin = SyntheticPlugin {
    id: "plugin-a",
    extensions: &["plug-a"],
    typing_policy: FrontendTypingPolicy::Strict,
    value: 11,
    reject_unknown: false,
    contexts: &PLUG_A_CONTEXTS,
};
static PLUG_B: SyntheticPlugin = SyntheticPlugin {
    id: "plugin-b",
    extensions: &["plug-b"],
    typing_policy: FrontendTypingPolicy::Dynamic,
    value: 22,
    reject_unknown: false,
    contexts: &PLUG_B_CONTEXTS,
};
static PLUG_C: SyntheticPlugin = SyntheticPlugin {
    id: "plugin-c",
    extensions: &["plug-c"],
    typing_policy: FrontendTypingPolicy::Strict,
    value: 33,
    reject_unknown: true,
    contexts: &PLUG_C_CONTEXTS,
};
static PLUG_D: SyntheticPlugin = SyntheticPlugin {
    id: "plugin-d",
    extensions: &["plug-d"],
    typing_policy: FrontendTypingPolicy::Dynamic,
    value: 44,
    reject_unknown: true,
    contexts: &PLUG_D_CONTEXTS,
};

struct PluginSourceFixture {
    path: PathBuf,
}

impl Deref for PluginSourceFixture {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl AsRef<Path> for PluginSourceFixture {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for PluginSourceFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

fn plugin_source_path(extension: &str) -> PluginSourceFixture {
    for _ in 0..32 {
        let id = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pd-vm-frontend-plugin-{}-{id}.{extension}",
            std::process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let fixture = PluginSourceFixture { path };
                let write_result = file.write_all(b"synthetic frontend input\n");
                drop(file);
                if let Err(err) = write_result {
                    drop(fixture);
                    panic!("contract source should write: {err}");
                }
                return fixture;
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => panic!("contract source should create: {err}"),
        }
    }
    panic!("could not allocate a unique frontend fixture path");
}

fn run_plugin(path: &Path, options: CompileSourceFileOptions, expected: i64) {
    let compiled = compile_source_file_with_options(path, options)
        .expect("registered plugin should compile its extension");
    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("compiled plugin program should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(expected)]);
}

#[test]
fn registered_plugins_dispatch_by_identity_and_extension_and_run() {
    let path_a = plugin_source_path("plug-a");
    let path_b = plugin_source_path("plug-b");
    run_plugin(
        &path_a,
        CompileSourceFileOptions::new().with_source_plugin(&PLUG_A),
        11,
    );
    run_plugin(
        &path_b,
        CompileSourceFileOptions::new().with_source_plugin(&PLUG_B),
        22,
    );

    let unregistered_extension =
        compile_source_file_with_options(&path_a, CompileSourceFileOptions::default())
            .err()
            .expect("an unregistered extension must be rejected");
    assert!(unregistered_extension.to_string().contains("plug-a"));

    let unregistered_identity = compile_source_with_flavor_and_options(
        "synthetic frontend input",
        SourceFlavor::Plugin(FrontendId::new("plugin-a")),
        CompileSourceFileOptions::new().with_source_plugin(&PLUG_B),
    )
    .err()
    .expect("an unregistered plugin identity must be rejected");
    assert!(unregistered_identity.to_string().contains("plugin-a"));
}

#[test]
fn plugin_context_preserves_source_phase_path_and_caller_catalog() {
    PLUG_A_CONTEXTS.lock().unwrap().clear();
    let catalog = std::sync::Arc::new(vm::HostApiCatalog::builder().build().unwrap());
    let catalog_ptr = std::sync::Arc::as_ptr(&catalog) as usize;
    let catalog_options = CompileSourceFileOptions::new()
        .with_host_api_catalog(std::sync::Arc::clone(&catalog))
        .with_source_plugin(&PLUG_A);

    let source_path = plugin_source_path("plug-a");
    let file_options = catalog_options.clone();
    run_plugin(&source_path, file_options, 11);
    compile_source_with_flavor_and_options(
        "synthetic frontend input",
        SourceFlavor::Plugin(FrontendId::new("plugin-a")),
        catalog_options,
    )
    .expect("standalone plugin source should compile");

    let seen = PLUG_A_CONTEXTS.lock().unwrap().clone();
    assert!(seen.iter().any(|context| {
        context.call == "imports"
            && context.source_id == 0
            && context.original_path.as_deref() == Some(source_path.path.as_path())
            && context.phase == ParsePhase::ImportDiscovery
            && context.catalog_ptr == Some(catalog_ptr)
            && context.import_count == 0
    }));
    assert!(seen.iter().any(|context| {
        context.call == "parse"
            && context.source_id == 0
            && context.original_path.as_deref() == Some(source_path.path.as_path())
            && context.phase == ParsePhase::Module
            && context.catalog_ptr == Some(catalog_ptr)
            && context.import_count == 0
    }));
    assert!(seen.iter().any(|context| {
        context.call == "parse"
            && context.source_id == 0
            && context.original_path.is_none()
            && context.phase == ParsePhase::Standalone
            && context.catalog_ptr == Some(catalog_ptr)
            && context.import_count == 0
    }));
}

#[test]
fn descriptor_typing_policy_rejects_unknown_schema_only_for_strict_plugins() {
    let strict = compile_source_with_flavor_and_options(
        "synthetic frontend input",
        SourceFlavor::Plugin(FrontendId::new("plugin-c")),
        CompileSourceFileOptions::new().with_source_plugin(&PLUG_C),
    )
    .err()
    .expect("strict plugin must reject unresolved typed IR");
    assert!(
        strict
            .to_string()
            .contains("strict typing requires concrete schemas")
    );

    let dynamic = compile_source_with_flavor_and_options(
        "synthetic frontend input",
        SourceFlavor::Plugin(FrontendId::new("plugin-d")),
        CompileSourceFileOptions::new().with_source_plugin(&PLUG_D),
    )
    .expect("dynamic plugin keeps its dynamic behavior");
    let mut vm = Vm::new(dynamic.program);
    assert_eq!(vm.run().unwrap(), VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(44)]);
}
