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

struct ResultIrPlugin;

static RESULT_IR: ResultIrPlugin = ResultIrPlugin;

fn result_schema() -> vm::compiler::TypeSchema {
    vm::compiler::TypeSchema::Result {
        ok: Box::new(vm::compiler::TypeSchema::Int),
        err: Box::new(vm::compiler::TypeSchema::Int),
    }
}

fn result_ir() -> FrontendIr {
    let span = vm::Span::new(0, 0, 1);
    FrontendIr {
        stmts: vec![
            Stmt::Let {
                index: 0,
                declared_schema: Some(result_schema()),
                expr: Expr::ResultConstruct {
                    ok: true,
                    payload: Box::new(Expr::Int(7)),
                    result_schema: result_schema(),
                    span,
                    semantic_id: None,
                },
                line: 1,
            },
            Stmt::Expr {
                expr: Expr::ResultMatch {
                    value: Box::new(Expr::MoveVar(0)),
                    value_slot: 1,
                    ok_binding: Some(2),
                    ok_expr: Box::new(Expr::Var(2)),
                    err_binding: Some(3),
                    err_expr: Box::new(Expr::Int(-1)),
                    scrutinee_schema: result_schema(),
                    span,
                    semantic_id: None,
                },
                line: 2,
            },
        ],
        locals: 4,
        local_bindings: Vec::new(),
        struct_schemas: Default::default(),
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

impl SourcePlugin for ResultIrPlugin {
    fn descriptor(&self) -> SourceFrontendDescriptor {
        SourceFrontendDescriptor {
            id: FrontendId::new("result-ir"),
            extensions: &["result-ir"],
            typing_policy: FrontendTypingPolicy::Strict,
            supports_ownership_checking: false,
        }
    }

    fn parse_source(
        &self,
        _source: &str,
        _context: &SourceFrontendContext<'_>,
    ) -> Result<FrontendIr, ParseError> {
        Ok(result_ir())
    }

    fn parse_module_imports(
        &self,
        _source: &str,
        _path: &Path,
        _context: &SourceFrontendContext<'_>,
    ) -> Result<Vec<vm::ModuleImport>, vm::SourcePathError> {
        Ok(Vec::new())
    }
}

#[test]
fn synthetic_frontend_ir_result_match_returns_ok_payload() {
    let path = plugin_source_path("result-ir");
    let compiled = compile_source_file_with_options(
        &path,
        CompileSourceFileOptions::new().with_source_plugin(&RESULT_IR),
    )
    .expect("synthetic Result IR should compile");
    let mut vm = Vm::new(compiled.program);
    assert_eq!(
        vm.run().expect("synthetic Result IR should run"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(7)]);
}

struct ContextualResultIrPlugin;

static CONTEXTUAL_RESULT_IR: ContextualResultIrPlugin = ContextualResultIrPlugin;

fn contextual_result_ir() -> FrontendIr {
    let span = vm::Span::new(0, 0, 1);
    let result = vm::TypeSchema::Result {
        ok: Box::new(vm::TypeSchema::Int),
        err: Box::new(vm::TypeSchema::String),
    };
    FrontendIr {
        stmts: vec![
            Stmt::ClosureLet {
                line: 1,
                expected_schema: Some(vm::TypeSchema::Callable {
                    params: Vec::new(),
                    result: Box::new(result.clone()),
                }),
                closure: vm::ClosureExpr {
                    param_slots: Vec::new(),
                    capture_copies: Vec::new(),
                    body: Box::new(Expr::ResultConstruct {
                        ok: false,
                        payload: Box::new(Expr::String("e".to_string())),
                        result_schema: vm::TypeSchema::Unknown,
                        span,
                        semantic_id: None,
                    }),
                },
            },
            Stmt::Let {
                index: 0,
                declared_schema: Some(vm::TypeSchema::Callable {
                    params: Vec::new(),
                    result: Box::new(result.clone()),
                }),
                expr: Expr::Closure(vm::ClosureExpr {
                    param_slots: Vec::new(),
                    capture_copies: Vec::new(),
                    body: Box::new(Expr::ResultConstruct {
                        ok: true,
                        payload: Box::new(Expr::Int(1)),
                        result_schema: vm::TypeSchema::Unknown,
                        span,
                        semantic_id: None,
                    }),
                }),
                line: 1,
            },
            Stmt::Let {
                index: 4,
                declared_schema: Some(vm::TypeSchema::Callable {
                    params: Vec::new(),
                    result: Box::new(result.clone()),
                }),
                expr: Expr::Closure(vm::ClosureExpr {
                    param_slots: Vec::new(),
                    capture_copies: Vec::new(),
                    body: Box::new(Expr::ResultConstruct {
                        ok: false,
                        payload: Box::new(Expr::String("e".to_string())),
                        result_schema: vm::TypeSchema::Unknown,
                        span,
                        semantic_id: None,
                    }),
                }),
                line: 1,
            },
            Stmt::Expr {
                expr: Expr::ResultMatch {
                    value: Box::new(Expr::LocalCall(0, Vec::new(), Vec::new(), None)),
                    value_slot: 1,
                    ok_binding: Some(2),
                    ok_expr: Box::new(Expr::Var(2)),
                    err_binding: Some(3),
                    err_expr: Box::new(Expr::Int(-1)),
                    scrutinee_schema: result.clone(),
                    span,
                    semantic_id: None,
                },
                line: 2,
            },
            Stmt::Expr {
                expr: Expr::ResultMatch {
                    value: Box::new(Expr::LocalCall(4, Vec::new(), Vec::new(), None)),
                    value_slot: 5,
                    ok_binding: Some(6),
                    ok_expr: Box::new(Expr::Var(6)),
                    err_binding: Some(7),
                    err_expr: Box::new(Expr::Int(-1)),
                    scrutinee_schema: result,
                    span,
                    semantic_id: None,
                },
                line: 3,
            },
        ],
        locals: 8,
        local_bindings: Vec::new(),
        struct_schemas: Default::default(),
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

impl SourcePlugin for ContextualResultIrPlugin {
    fn descriptor(&self) -> SourceFrontendDescriptor {
        SourceFrontendDescriptor {
            id: FrontendId::new("contextual-result-ir"),
            extensions: &["contextual-result-ir"],
            typing_policy: FrontendTypingPolicy::Strict,
            supports_ownership_checking: false,
        }
    }

    fn parse_source(
        &self,
        _source: &str,
        _context: &SourceFrontendContext<'_>,
    ) -> Result<FrontendIr, ParseError> {
        Ok(contextual_result_ir())
    }

    fn parse_module_imports(
        &self,
        _source: &str,
        _path: &Path,
        _context: &SourceFrontendContext<'_>,
    ) -> Result<Vec<vm::ModuleImport>, vm::SourcePathError> {
        Ok(Vec::new())
    }
}

#[test]
fn synthetic_frontend_ir_closure_result_uses_expected_callable_schema() {
    let path = plugin_source_path("contextual-result-ir");
    let compiled = compile_source_file_with_options(
        &path,
        CompileSourceFileOptions::new().with_source_plugin(&CONTEXTUAL_RESULT_IR),
    )
    .expect("typed closure Result IR should compile from expected schema");
    let mut vm = Vm::new(compiled.program);
    assert_eq!(
        vm.run().expect("typed closure Result IR should run"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(1), Value::Int(-1)]);
}

struct CapturedResultIrPlugin;
static CAPTURED_RESULT_IR: CapturedResultIrPlugin = CapturedResultIrPlugin;

fn captured_result_ir() -> FrontendIr {
    let span = vm::Span::new(0, 0, 1);
    let result = vm::TypeSchema::Result {
        ok: Box::new(vm::TypeSchema::Int),
        err: Box::new(vm::TypeSchema::String),
    };
    let callable = vm::TypeSchema::Callable {
        params: Vec::new(),
        result: Box::new(result.clone()),
    };
    let source_slot = 0;
    let captured_slot = 1;
    let closure = || vm::ClosureExpr {
        param_slots: Vec::new(),
        capture_copies: vec![(source_slot, captured_slot)],
        body: Box::new(Expr::ResultConstruct {
            ok: true,
            payload: Box::new(Expr::Var(captured_slot)),
            result_schema: vm::TypeSchema::Unknown,
            span,
            semantic_id: None,
        }),
    };
    FrontendIr {
        stmts: vec![
            Stmt::Let {
                index: source_slot,
                declared_schema: Some(vm::TypeSchema::Int),
                expr: Expr::Int(7),
                line: 1,
            },
            Stmt::ClosureLet {
                line: 1,
                expected_schema: Some(callable.clone()),
                closure: closure(),
            },
            Stmt::Let {
                index: 2,
                declared_schema: Some(callable),
                expr: Expr::Closure(closure()),
                line: 2,
            },
            Stmt::Expr {
                expr: Expr::ResultMatch {
                    value: Box::new(Expr::LocalCall(2, Vec::new(), Vec::new(), None)),
                    value_slot: 3,
                    ok_binding: Some(4),
                    ok_expr: Box::new(Expr::Var(4)),
                    err_binding: Some(5),
                    err_expr: Box::new(Expr::Int(-1)),
                    scrutinee_schema: result,
                    span,
                    semantic_id: None,
                },
                line: 3,
            },
        ],
        locals: 6,
        local_bindings: Vec::new(),
        struct_schemas: Default::default(),
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

impl SourcePlugin for CapturedResultIrPlugin {
    fn descriptor(&self) -> SourceFrontendDescriptor {
        SourceFrontendDescriptor {
            id: FrontendId::new("captured-result-ir"),
            extensions: &["captured-result-ir"],
            typing_policy: FrontendTypingPolicy::Strict,
            supports_ownership_checking: false,
        }
    }

    fn parse_source(
        &self,
        _source: &str,
        _context: &SourceFrontendContext<'_>,
    ) -> Result<FrontendIr, ParseError> {
        Ok(captured_result_ir())
    }

    fn parse_module_imports(
        &self,
        _source: &str,
        _path: &Path,
        _context: &SourceFrontendContext<'_>,
    ) -> Result<Vec<vm::ModuleImport>, vm::SourcePathError> {
        Ok(Vec::new())
    }
}

#[test]
fn contextual_captured_result_ir_executes_distinct_capture_slots() {
    let ir = captured_result_ir();
    let copies = match &ir.stmts[2] {
        Stmt::Let {
            expr: Expr::Closure(closure),
            ..
        } => closure.capture_copies.clone(),
        other => panic!("executable closure missing: {other:?}"),
    };
    assert_eq!(copies, vec![(0, 1)]);
    assert_ne!(copies[0].0, copies[0].1);
    let path = plugin_source_path("captured-result-ir");
    let compiled = compile_source_file_with_options(
        &path,
        CompileSourceFileOptions::new().with_source_plugin(&CAPTURED_RESULT_IR),
    )
    .expect("captured Result IR should compile");
    let mut vm = Vm::new(compiled.program);
    assert_eq!(
        vm.run().expect("captured Result IR should run"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(7)]);
}

struct GuestBoxClosurePlugin;
static GUEST_BOX_CLOSURE: GuestBoxClosurePlugin = GuestBoxClosurePlugin;

fn builtin_call(builtin: vm::BuiltinFunction, args: Vec<Expr>) -> Expr {
    Expr::Call(builtin.call_index(), Vec::new(), args, None, None)
}

fn unknown_ok(payload: Expr) -> Expr {
    Expr::ResultConstruct {
        ok: true,
        payload: Box::new(payload),
        result_schema: vm::TypeSchema::Unknown,
        span: vm::Span::new(0, 0, 1),
        semantic_id: None,
    }
}

fn guest_box_body(field: &str, payload: Expr) -> Expr {
    unknown_ok(builtin_call(
        vm::BuiltinFunction::Set,
        vec![
            builtin_call(vm::BuiltinFunction::MapNew, Vec::new()),
            Expr::String(field.to_string()),
            unknown_ok(payload),
        ],
    ))
}

fn guest_box_ir(field: &str, payload: Expr) -> FrontendIr {
    let span = vm::Span::new(0, 0, 1);
    let inner = vm::TypeSchema::Result {
        ok: Box::new(vm::TypeSchema::Int),
        err: Box::new(vm::TypeSchema::String),
    };
    let named = vm::TypeSchema::Named("Box".to_string(), Vec::new());
    let outer = vm::TypeSchema::Result {
        ok: Box::new(named.clone()),
        err: Box::new(vm::TypeSchema::String),
    };
    let callable = vm::TypeSchema::Callable {
        params: Vec::new(),
        result: Box::new(outer.clone()),
    };
    let body = guest_box_body(field, payload);
    FrontendIr {
        stmts: vec![
            Stmt::ClosureLet {
                line: 1,
                expected_schema: Some(callable.clone()),
                closure: vm::ClosureExpr {
                    param_slots: Vec::new(),
                    capture_copies: Vec::new(),
                    body: Box::new(body.clone()),
                },
            },
            Stmt::Let {
                index: 0,
                declared_schema: Some(callable),
                expr: Expr::Closure(vm::ClosureExpr {
                    param_slots: Vec::new(),
                    capture_copies: Vec::new(),
                    body: Box::new(body),
                }),
                line: 2,
            },
            Stmt::Expr {
                expr: Expr::ResultMatch {
                    value: Box::new(Expr::LocalCall(0, Vec::new(), Vec::new(), None)),
                    value_slot: 1,
                    ok_binding: Some(2),
                    ok_expr: Box::new(Expr::ResultMatch {
                        value: Box::new(builtin_call(
                            vm::BuiltinFunction::Get,
                            vec![Expr::Var(2), Expr::String("value".to_string())],
                        )),
                        value_slot: 3,
                        ok_binding: Some(4),
                        ok_expr: Box::new(Expr::Var(4)),
                        err_binding: Some(5),
                        err_expr: Box::new(Expr::Int(-1)),
                        scrutinee_schema: inner,
                        span,
                        semantic_id: None,
                    }),
                    err_binding: Some(6),
                    err_expr: Box::new(Expr::Int(-2)),
                    scrutinee_schema: outer,
                    span,
                    semantic_id: None,
                },
                line: 3,
            },
        ],
        locals: 7,
        local_bindings: Vec::new(),
        struct_schemas: std::iter::once((
            "Box".to_string(),
            vm::compiler::ir::StructDecl {
                name: "Box".to_string(),
                type_params: Vec::new(),
                body_schema: vm::TypeSchema::Object(
                    std::iter::once(("value".to_string(), {
                        vm::TypeSchema::Result {
                            ok: Box::new(vm::TypeSchema::Int),
                            err: Box::new(vm::TypeSchema::String),
                        }
                    }))
                    .collect(),
                ),
                origin: vm::compiler::StructDeclOrigin::Guest,
            },
        ))
        .collect(),
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

impl SourcePlugin for GuestBoxClosurePlugin {
    fn descriptor(&self) -> SourceFrontendDescriptor {
        SourceFrontendDescriptor {
            id: FrontendId::new("guest-box-closure"),
            extensions: &["guest-box"],
            typing_policy: FrontendTypingPolicy::Strict,
            supports_ownership_checking: false,
        }
    }

    fn parse_source(
        &self,
        source: &str,
        _context: &SourceFrontendContext<'_>,
    ) -> Result<FrontendIr, ParseError> {
        if source.contains("wrong-field") {
            return Ok(guest_box_ir("other", Expr::Int(1)));
        }
        if source.contains("wrong-payload") {
            return Ok(guest_box_ir("value", Expr::String("no".to_string())));
        }
        Ok(guest_box_ir("value", Expr::Int(1)))
    }

    fn parse_module_imports(
        &self,
        _source: &str,
        _path: &Path,
        _context: &SourceFrontendContext<'_>,
    ) -> Result<Vec<vm::ModuleImport>, vm::SourcePathError> {
        Ok(Vec::new())
    }
}

fn guest_box_source(marker: &str) -> PluginSourceFixture {
    let path = plugin_source_path("guest-box");
    std::fs::write(&path.path, format!("{marker}\n")).expect("guest box source");
    path
}

#[test]
fn closure_let_guest_named_expected_schema_survives_canonical_remap() {
    let path = guest_box_source("ok");
    let compiled = compile_source_file_with_options(
        &path,
        CompileSourceFileOptions::new().with_source_plugin(&GUEST_BOX_CLOSURE),
    )
    .expect("guest Box ClosureLet should compile after canonical rename");
    let decls = compiled.program.named_struct_decls();
    let (name, decl) = decls
        .iter()
        .find(|(name, _)| name.starts_with("__guest_type_"))
        .expect("canonical guest declaration");
    assert_eq!(decl.name, name.as_str());
    assert_ne!(*name, "Box");
    let vm::TypeSchema::Object(fields) = &decl.body_schema else {
        panic!("guest body should stay an object: {:?}", decl.body_schema);
    };
    match fields.get("value") {
        Some(vm::TypeSchema::Result { ok, err }) => {
            assert!(matches!(ok.as_ref(), vm::TypeSchema::Int));
            assert!(matches!(err.as_ref(), vm::TypeSchema::String));
        }
        other => panic!("nested field schema was {other:?}"),
    }
    let mut vm = Vm::new(compiled.program);
    assert_eq!(
        vm.run().expect("guest Box closure should run"),
        VmStatus::Halted
    );
    assert_eq!(vm.stack(), &[Value::Int(1)]);

    for (marker, label) in [("wrong-field", "field"), ("wrong-payload", "payload")] {
        let path = guest_box_source(marker);
        let error = match compile_source_file_with_options(
            &path,
            CompileSourceFileOptions::new().with_source_plugin(&GUEST_BOX_CLOSURE),
        ) {
            Err(error) => error,
            Ok(_) => panic!("{label} must reject"),
        };
        let text = error.to_string();
        let rejected = text.contains("schema")
            || text.contains("field")
            || text.contains("string")
            || text.contains("assigned")
            || text.contains("annotation")
            || text.contains("inactive");
        assert!(rejected, "{label} diagnostic was {text}");
        if label == "payload" {
            assert!(
                text.contains("schema") || text.contains("string") || text.contains("assigned"),
                "payload diagnostic was {text}"
            );
        }
    }
}
