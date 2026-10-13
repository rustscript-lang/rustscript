//! Public three-field CLI consumer: file registration does not select a REPL producer.
#[cfg(feature = "cli")]
mod consumer {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use vm::{
        CompileSourceFileOptions, FrontendIr, ParseError, SourceFrontendContext, SourcePlugin,
    };

    struct FileFrontend;
    static FILE_FRONTEND: FileFrontend = FileFrontend;
    static HOOKS: AtomicUsize = AtomicUsize::new(0);
    impl SourcePlugin for FileFrontend {
        fn descriptor(&self) -> vm::SourceFrontendDescriptor {
            vm::SourceFrontendDescriptor {
                id: vm::FrontendId::new("external-file-probe"),
                extensions: &["external"],
                typing_policy: vm::FrontendTypingPolicy::Strict,
                supports_ownership_checking: true,
            }
        }
        fn parse_source(
            &self,
            _: &str,
            _: &SourceFrontendContext<'_>,
        ) -> Result<FrontendIr, ParseError> {
            HOOKS.fetch_add(1, Ordering::SeqCst);
            Err(ParseError::new(
                "this fixture only exercises native REPL selection",
            ))
        }
        fn validate_typed_ir(&self, _: &vm::FrontendTypedIr<'_>) -> Result<(), vm::CompileError> {
            HOOKS.fetch_add(1, Ordering::SeqCst);
            panic!("unselected file hook must not run")
        }
    }

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        vm::cli::main(vm::cli::CliRuntime {
            binary_name: "cli-registered-frontend",
            default_source: "unused.rss",
            compile_options: || {
                CompileSourceFileOptions::default().with_source_plugin(&FILE_FRONTEND)
            },
        })?;
        assert_eq!(HOOKS.load(Ordering::SeqCst), 0);
        println!("external-file-hooks=0");
        Ok(())
    }
}

#[cfg(feature = "cli")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    consumer::run()
}
#[cfg(not(feature = "cli"))]
fn main() {}
