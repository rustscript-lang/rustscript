const fs = require("node:fs");
const path = require("node:path");
const { spawnSync } = require("node:child_process");

const extensionRoot = path.resolve(__dirname, "..");
const repoRoot = path.resolve(extensionRoot, "..", "..");
const wasmTarget = "wasm32-unknown-unknown";
const wasmName = "pd_vm_lint_wasm.wasm";
const compiledWasmPath = path.resolve(
  repoRoot,
  "target",
  wasmTarget,
  "release",
  wasmName
);
const outputDir = path.join(extensionRoot, "wasm");
const outputPath = path.join(outputDir, wasmName);

function run(command, args, cwd) {
  const result = spawnSync(command, args, {
    cwd,
    stdio: "inherit",
    shell: process.platform === "win32"
  });
  if (result.status !== 0) {
    process.exit(result.status ?? 1);
  }
}

run("rustup", ["target", "add", wasmTarget], repoRoot);
run(
  "cargo",
  ["build", "-p", "pd-vm-lint-wasm", "--target", wasmTarget, "--release"],
  repoRoot
);

if (!fs.existsSync(compiledWasmPath)) {
  console.error(`built lint wasm not found: ${compiledWasmPath}`);
  process.exitCode = 1;
} else {
  fs.mkdirSync(outputDir, { recursive: true });
  fs.copyFileSync(compiledWasmPath, outputPath);
  console.log(`copied lint wasm: ${outputPath}`);
}
