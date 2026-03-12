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
const syncScript = resolve(repoRoot, "scripts", "sync-editor-wasm.mjs");

run("node", [syncScript, "controller"], repoRoot);
