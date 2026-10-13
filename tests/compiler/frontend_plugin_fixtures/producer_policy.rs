use super::*;
use std::sync::Mutex;

static VALIDATIONS: Mutex<Vec<(String, u32)>> = Mutex::new(Vec::new());
static SERIAL: Mutex<()> = Mutex::new(());
struct ProducerPolicy {
    id: &'static str,
    extension: &'static [&'static str],
    strict: bool,
}
static STRICT: ProducerPolicy = ProducerPolicy {
    id: "neutral-policy-strict",
    extension: &["checked"],
    strict: true,
};
static DYNAMIC: ProducerPolicy = ProducerPolicy {
    id: "neutral-policy-dynamic",
    extension: &["flow"],
    strict: false,
};
impl SourcePlugin for ProducerPolicy {
    fn descriptor(&self) -> SourceFrontendDescriptor {
        SourceFrontendDescriptor {
            id: FrontendId::new(self.id),
            extensions: self.extension,
            typing_policy: if self.strict {
                FrontendTypingPolicy::Strict
            } else {
                FrontendTypingPolicy::Dynamic
            },
            supports_ownership_checking: self.strict,
        }
    }
    fn parse_module_imports(
        &self,
        source: &str,
        path: &Path,
        context: &SourceFrontendContext<'_>,
    ) -> Result<Vec<ModuleImport>, vm::SourcePathError> {
        graph_format::GRAPH_FORMAT.parse_module_imports(source, path, context)
    }
    fn parse_source(
        &self,
        source: &str,
        context: &SourceFrontendContext<'_>,
    ) -> Result<FrontendIr, ParseError> {
        graph_format::GRAPH_FORMAT.parse_source(source, context)
    }
    fn module_type_exports(
        &self,
        source: &str,
        context: &SourceFrontendContext<'_>,
        ir: &FrontendIr,
    ) -> Result<Vec<vm::compiler::FrontendTypeExport>, ParseError> {
        graph_format::GRAPH_FORMAT.module_type_exports(source, context, ir)
    }
    fn validate_typed_ir(&self, typed: &vm::FrontendTypedIr<'_>) -> Result<(), vm::CompileError> {
        let index = typed.ir.parsed_semantic_index.as_ref().unwrap();
        let owner = typed.source_id.expect("source-owned validation identity");
        assert!(
            index
                .scopes
                .iter()
                .any(|scope| scope.range.source_id == owner)
        );
        VALIDATIONS
            .lock()
            .unwrap()
            .push((self.id.to_string(), owner));
        Ok(())
    }
}
fn options() -> CompileSourceFileOptions {
    CompileSourceFileOptions::new()
        .with_source_plugin(&STRICT)
        .with_source_plugin(&DYNAMIC)
}
#[test]
fn strict_dependency_cannot_be_erased_by_dynamic_root_and_dynamic_dependency_remains_dynamic() {
    let _lock = SERIAL.lock().unwrap();
    let root = temp_root("producer-policy");
    let entry = root.join("main.flow");
    write_source(
        &root.join("lib.checked"),
        "pub fn bad(value: unknown) -> int { 42 }\n",
    );
    let source = "bind value bad bad ./lib.checked\nbad(1);\n";
    write_source(&entry, source);
    assert!(
        compile_source_file_with_options(&entry, options()).is_err(),
        "dynamic root cannot erase strict dependency annotations"
    );
    assert!(vm::compiler::analyze_source_file_with_options(&entry, options()).is_err());
    assert!(
        vm::collect_inferred_local_type_hints_at_path_with_options(
            &entry,
            source,
            vm::SourceFlavor::Plugin(FrontendId::new(DYNAMIC.id)),
            options()
        )
        .is_err()
    );
    let entry = root.join("main.checked");
    write_source(
        &root.join("lib.flow"),
        "pub fn dynamic(value: unknown) -> int { 42 }\n",
    );
    let source = "bind value dynamic dynamic ./lib.flow\ndynamic(1);\n";
    write_source(&entry, source);
    let compiled = compile_source_file_with_options(&entry, options())
        .expect("strict root must not impose its policy on dynamic producer");
    let mut machine = Vm::new(compiled.program);
    assert_eq!(machine.run().unwrap(), VmStatus::Halted);
    assert_eq!(machine.stack(), &[Value::Int(42)]);
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn each_dependency_validator_runs_once_for_compile_analyze_hint_including_native_root() {
    let _lock = SERIAL.lock().unwrap();
    let root = temp_root("producer-hooks");
    write_source(
        &root.join("lib.checked"),
        "pub fn plus(value: int) -> int { value + 1 }\n",
    );
    let entry = root.join("main.rss");
    let source = "use self::lib as lib;\nlib::plus(41);\n";
    let configured = || options().with_module_override_path("./lib.rss", root.join("lib.checked"));
    write_source(&entry, source);
    for operation in 0..3 {
        VALIDATIONS.lock().unwrap().clear();
        match operation {
            0 => {
                let compiled = compile_source_file_with_options(&entry, configured()).unwrap();
                let mut machine = Vm::new(compiled.program);
                assert_eq!(machine.run().unwrap(), VmStatus::Halted);
                assert_eq!(machine.stack(), &[Value::Int(42)]);
            }
            1 => {
                vm::compiler::analyze_source_file_with_options(&entry, configured()).unwrap();
            }
            _ => {
                vm::collect_inferred_local_type_hints_at_path_with_options(
                    &entry,
                    source,
                    vm::SourceFlavor::RustScript,
                    configured(),
                )
                .unwrap();
            }
        }
        assert_eq!(
            *VALIDATIONS.lock().unwrap(),
            vec![(STRICT.id.to_string(), 1)],
            "operation {operation}"
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}
