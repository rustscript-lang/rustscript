use std::path::{Path, PathBuf};

use vm::{
    CompileSourceFileOptions, CompiledProgram, SourcePathError, compile_source_file_with_options,
};

pub const EDGE_ASYNC_IO_MODULE_SPEC: &str = "edge/io_async.rss";

pub fn edge_async_io_module_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("stdlib/rss/io_async.rss")
}

pub fn edge_compile_options() -> CompileSourceFileOptions {
    let mut options = CompileSourceFileOptions::new();
    options.set_module_override_path(EDGE_ASYNC_IO_MODULE_SPEC, edge_async_io_module_path());
    options
}

pub fn compile_edge_source_file(path: impl AsRef<Path>) -> Result<CompiledProgram, SourcePathError> {
    compile_edge_source_file_with_options(path, CompileSourceFileOptions::new())
}

pub fn compile_edge_source_file_with_options(
    path: impl AsRef<Path>,
    mut options: CompileSourceFileOptions,
) -> Result<CompiledProgram, SourcePathError> {
    if options
        .module_override_path(EDGE_ASYNC_IO_MODULE_SPEC)
        .is_none()
    {
        options.set_module_override_path(EDGE_ASYNC_IO_MODULE_SPEC, edge_async_io_module_path());
    }
    compile_source_file_with_options(path, options)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_async_io_module_exists() {
        let path = edge_async_io_module_path();
        assert!(path.exists(), "edge async io module is missing: {path:?}");
    }

    #[test]
    fn edge_compile_options_include_default_async_io_override() {
        let options = edge_compile_options();
        assert!(
            options
                .module_override_path(EDGE_ASYNC_IO_MODULE_SPEC)
                .is_some(),
            "edge compile options should include io_async module override"
        );
    }
}
