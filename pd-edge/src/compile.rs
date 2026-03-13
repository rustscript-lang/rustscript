use std::path::Path;

use vm::{
    CompileSourceFileOptions, CompiledProgram, SourcePathError, compile_source_file_with_options,
};

fn with_edge_stdlib_overrides(mut options: CompileSourceFileOptions) -> CompileSourceFileOptions {
    options.set_module_override_source(
        "edge/http/upstream/request.rss",
        include_str!("../stdlib/rss/http/upstream/request.rss"),
    );
    options.set_module_override_source(
        "edge/http/upstream/response.rss",
        include_str!("../stdlib/rss/http/upstream/response.rss"),
    );
    options.set_module_override_source(
        "edge/http/upstream.rss",
        include_str!("../stdlib/rss/http/upstream.rss"),
    );
    options
}

pub fn compile_edge_source_file(
    path: impl AsRef<Path>,
) -> Result<CompiledProgram, SourcePathError> {
    compile_edge_source_file_with_options(path, CompileSourceFileOptions::new())
}

pub fn compile_edge_source_file_with_options(
    path: impl AsRef<Path>,
    options: CompileSourceFileOptions,
) -> Result<CompiledProgram, SourcePathError> {
    compile_source_file_with_options(path, with_edge_stdlib_overrides(options))
}
