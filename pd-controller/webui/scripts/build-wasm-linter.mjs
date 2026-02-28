import { mkdirSync, copyFileSync, existsSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

function run(command, args, cwd) {
  const result = spawnSync(command, args, {
    cwd,
    stdio: "inherit",
    shell: process.platform === "win32"
  });
  if (result.status !== 0) {
    throw new Error(`${command} ${args.join(" ")} failed with exit code ${result.status ?? -1}`);
  }
}

const thisFile = fileURLToPath(import.meta.url);
const scriptsDir = dirname(thisFile);
const webuiDir = resolve(scriptsDir, "..");
const repoRoot = resolve(webuiDir, "..", "..");
const wasmTarget = "wasm32-unknown-unknown";
const wasmName = "pd_vm_lint_wasm.wasm";
const wasmSrc = resolve(repoRoot, "target", wasmTarget, "release", wasmName);
const wasmOutDir = resolve(webuiDir, "public", "wasm");
const wasmOut = resolve(wasmOutDir, wasmName);

run("rustup", ["target", "add", wasmTarget], repoRoot);
run("cargo", ["build", "-p", "pd-vm-lint-wasm", "--target", wasmTarget, "--release"], repoRoot);

if (!existsSync(wasmSrc)) {
  throw new Error(`expected wasm output not found: ${wasmSrc}`);
}

mkdirSync(wasmOutDir, { recursive: true });
copyFileSync(wasmSrc, wasmOut);
console.log(`copied wasm linter to ${wasmOut}`);
