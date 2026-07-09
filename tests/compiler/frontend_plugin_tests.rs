#[path = "../common/mod.rs"]
mod common;
use common::*;
use std::collections::HashMap;
use vm::{
    CompileSourceFileOptions, FrontendImportSyntax, FrontendIr, ParseError, SourceFlavor,
    SourcePlugin, compile_source_with_flavor_and_options,
};

struct ConstantPlugin;

impl SourcePlugin for ConstantPlugin {
    fn flavor(&self) -> SourceFlavor {
        SourceFlavor::JavaScript
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["js"]
    }

    fn import_syntax(&self) -> FrontendImportSyntax {
        FrontendImportSyntax::JavaScript
    }

    fn parse_source(&self, _source: &str) -> Result<FrontendIr, ParseError> {
        Ok(FrontendIr {
            stmts: vec![Stmt::Expr {
                expr: Expr::Int(7),
                line: 1,
            }],
            locals: 0,
            local_bindings: Vec::new(),
            struct_schemas: HashMap::new(),
            unknown_type_spans: Vec::new(),
            functions: Vec::new(),
            function_impls: HashMap::new(),
            stmt_sources: Vec::new(),
            function_sources: HashMap::new(),
        })
    }
}

static CONSTANT_PLUGIN: ConstantPlugin = ConstantPlugin;

#[test]
fn registered_compat_frontend_plugin_compiles_source() {
    let options = CompileSourceFileOptions::new().with_source_plugin(&CONSTANT_PLUGIN);
    let compiled =
        compile_source_with_flavor_and_options("ignored();", SourceFlavor::JavaScript, options)
            .expect("registered plugin should compile JavaScript flavor");

    let mut vm = Vm::new(compiled.program);
    let status = vm.run().expect("compiled plugin program should run");
    assert_eq!(status, VmStatus::Halted);
    assert_eq!(vm.stack(), &[Value::Int(7)]);
}
