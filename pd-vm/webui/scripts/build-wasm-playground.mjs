import { copyFileSync, existsSync, mkdirSync } from "node:fs";
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
const pdVmDir = resolve(webuiDir, "..");
const repoRoot = resolve(pdVmDir, "..");

const wasmTarget = "wasm32-unknown-unknown";
const wasmName = "pd_vm_runtime_wasm.wasm";
const wasmSrc = resolve(repoRoot, "target", wasmTarget, "release", wasmName);
const wasmOutDir = resolve(webuiDir, "public", "wasm");
const wasmOut = resolve(wasmOutDir, wasmName);

const rssExtensionDir = resolve(repoRoot, ".vscode", "rss-language-extension");
const rssGrammarSrc = resolve(rssExtensionDir, "syntaxes", "rss.tmLanguage.json");
const rssConfigSrc = resolve(rssExtensionDir, "language-configuration.json");
const monacoConfigDir = resolve(webuiDir, "src", "monaco");
const rssGrammarOut = resolve(monacoConfigDir, "rss.tmLanguage.json");
const rssConfigOut = resolve(monacoConfigDir, "rss.language-configuration.json");

run("rustup", ["target", "add", wasmTarget], repoRoot);
run("cargo", ["build", "-p", "pd-vm-runtime-wasm", "--target", wasmTarget, "--release"], repoRoot);

if (!existsSync(wasmSrc)) {
  throw new Error(`expected wasm output not found: ${wasmSrc}`);
}

mkdirSync(wasmOutDir, { recursive: true });
copyFileSync(wasmSrc, wasmOut);
console.log(`copied wasm playground runtime to ${wasmOut}`);

if (!existsSync(rssGrammarSrc)) {
  throw new Error(`expected RSS grammar not found: ${rssGrammarSrc}`);
}
if (!existsSync(rssConfigSrc)) {
  throw new Error(`expected RSS language config not found: ${rssConfigSrc}`);
}

mkdirSync(monacoConfigDir, { recursive: true });
copyFileSync(rssGrammarSrc, rssGrammarOut);
copyFileSync(rssConfigSrc, rssConfigOut);
console.log(`synced RSS Monaco grammar to ${rssGrammarOut}`);
