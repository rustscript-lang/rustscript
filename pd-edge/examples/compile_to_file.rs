use std::{io, path::PathBuf};

use edge::{compile_edge_source_file, function_by_name};
use vm::{HostImport, encode_program};

/// Compile an .rss source file and write the encoded .vmbc bytes to an output path.
/// Usage: cargo run --example compile_to_file -- <source.rss> <output.vmbc>
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let source_rel = args
        .next()
        .ok_or_else(|| io::Error::other("usage: compile_to_file <source.rss> <output.vmbc>"))?;
    let output_path = args
        .next()
        .ok_or_else(|| io::Error::other("usage: compile_to_file <source.rss> <output.vmbc>"))?;

    let source_path = PathBuf::from(&source_rel);
    let compiled = compile_edge_source_file(&source_path)?;

    ensure_edge_abi(&compiled.program.imports)?;

    let payload = encode_program(&compiled.program)?;
    std::fs::write(&output_path, &payload)?;
    println!(
        "compiled {} -> {} ({} bytes)",
        source_rel,
        output_path,
        payload.len()
    );
    Ok(())
}

fn ensure_edge_abi(imports: &[HostImport]) -> Result<(), io::Error> {
    for import in imports {
        let Some(abi) = function_by_name(&import.name) else {
            return Err(io::Error::other(format!(
                "unknown proxy host import '{}'",
                import.name
            )));
        };
        if import.arity != abi.arity {
            return Err(io::Error::other(format!(
                "function ABI mismatch for '{}': expected arity {}, got {}",
                abi.name, abi.arity, import.arity
            )));
        }
    }
    Ok(())
}
