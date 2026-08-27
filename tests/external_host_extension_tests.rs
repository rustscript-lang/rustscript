//! Runs the genuinely separate host-extension fixture through the ordinary
//! integration-test path.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn external_host_extension_fixture_is_automated() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let manifest = repo.join("tests/fixtures/external-host-extension/Cargo.toml");
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("rustscript-external-host-extension-target"));

    let output = Command::new("cargo")
        .arg("test")
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--locked")
        .arg("--all-targets")
        .arg("--all-features")
        .env("CARGO_TARGET_DIR", target_dir)
        .env("CARGO_TERM_COLOR", "never")
        .output()
        .expect("cargo must be available to run the external fixture");

    assert!(
        output.status.success(),
        "external host-extension fixture failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
