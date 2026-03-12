use std::{io, path::PathBuf};

use edge::{ABI_VERSION, HOST_FUNCTION_COUNT, compile_edge_source_file, function_by_name};
use reqwest::StatusCode;
use vm::{HostImport, encode_program, validate_program};

// Compile an example source file, validate it against the current host ABI, then upload it.
const SOURCE_PATH: &str = "examples/sample_proxy_program.rss";
const CONTROL_URL: &str = "http://127.0.0.1:8081/program";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source_rel = std::env::args()
        .nth(1)
        .unwrap_or_else(|| SOURCE_PATH.to_string());
    let source_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(source_rel);
    let compiled = compile_edge_source_file(&source_path)?;

    // Fail early if the program targets stale or mismatched host imports.
    ensure_edge_abi(&compiled.program.imports)?;
    validate_program(&compiled.program, HOST_FUNCTION_COUNT)?;

    // Upload the encoded program to the local control plane once validation succeeds.
    let payload = encode_program(&compiled.program)?;
    let client = reqwest::Client::new();
    let response = client
        .put(CONTROL_URL)
        .header("content-type", "application/octet-stream")
        .body(payload)
        .send()
        .await?;

    let status = response.status();
    let body = response.text().await?;
    if status != StatusCode::NO_CONTENT {
        return Err(
            io::Error::other(format!("upload failed: status={status}, body={body}",)).into(),
        );
    }

    println!(
        "compiled and uploaded source from {}",
        source_path.display()
    );
    println!("proxy abi version: {ABI_VERSION}");
    println!("control response: {status}");
    Ok(())
}

fn ensure_edge_abi(imports: &[HostImport]) -> Result<(), io::Error> {
    // Keep upload failures actionable by checking every imported host function up front.
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
