use std::path::Path;

use vm::{
    CompileSourceFileOptions, CompiledProgram, SourcePathError, compile_source_file_with_options,
};

pub fn compile_edge_source_file(
    path: impl AsRef<Path>,
) -> Result<CompiledProgram, SourcePathError> {
    compile_edge_source_file_with_options(path, CompileSourceFileOptions::new())
}

pub fn compile_edge_source_file_with_options(
    path: impl AsRef<Path>,
    options: CompileSourceFileOptions,
) -> Result<CompiledProgram, SourcePathError> {
    compile_source_file_with_options(path, options)
}
