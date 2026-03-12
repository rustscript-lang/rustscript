import { copyFileSync, existsSync, mkdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const ALL_TARGETS = ["extension", "controller", "playground"];

const thisFile = fileURLToPath(import.meta.url);
const scriptsDir = dirname(thisFile);
const repoRoot = resolve(scriptsDir, "..");

const wasmTarget = "wasm32-unknown-unknown";
const wasmName = "pd_vm_wasm.wasm";
const compiledWasmPath = resolve(
  repoRoot,
  "target",
  wasmTarget,
  "release",
  wasmName
);

const rssExtensionDir = resolve(repoRoot, ".vscode", "rss-language-extension");
const rssGrammarSrc = resolve(rssExtensionDir, "syntaxes", "rss.tmLanguage.json");
const rssConfigSrc = resolve(rssExtensionDir, "language-configuration.json");

const targetSpecs = {
  extension: {
    needsRuntime: false,
    wasmOut: resolve(rssExtensionDir, "wasm", wasmName)
  },
  controller: {
    needsRuntime: false,
    wasmOut: resolve(repoRoot, "pd-controller", "webui", "public", "wasm", wasmName),
    grammarOutDir: resolve(repoRoot, "pd-controller", "webui", "src", "app", "monaco")
  },
  playground: {
    needsRuntime: true,
    wasmOut: resolve(repoRoot, "pd-vm", "webui", "public", "wasm", wasmName),
    grammarOutDir: resolve(repoRoot, "pd-vm", "webui", "src", "monaco")
  }
};

function run(command, args) {
  const result = spawnSync(command, args, {
    cwd: repoRoot,
    stdio: "inherit",
    shell: process.platform === "win32"
  });
  if (result.status !== 0) {
    throw new Error(`${command} ${args.join(" ")} failed with exit code ${result.status ?? -1}`);
  }
}

function normalizeTargets(rawTargets) {
  if (rawTargets.length === 0 || rawTargets.includes("all")) {
    return ALL_TARGETS;
  }

  const uniqueTargets = [...new Set(rawTargets)];
  for (const target of uniqueTargets) {
    if (!ALL_TARGETS.includes(target)) {
      throw new Error(
        `unknown sync target '${target}'. Expected one of: ${[...ALL_TARGETS, "all"].join(", ")}`
      );
    }
  }
  return uniqueTargets;
}

function ensureFileExists(path, label) {
  if (!existsSync(path)) {
    throw new Error(`${label} not found: ${path}`);
  }
}

function copyFileTo(pathFrom, pathTo, label) {
  mkdirSync(dirname(pathTo), { recursive: true });
  copyFileSync(pathFrom, pathTo);
  console.log(`${label}: ${pathTo}`);
}

function copyGrammarAssets(grammarOutDir) {
  ensureFileExists(rssGrammarSrc, "RSS grammar");
  ensureFileExists(rssConfigSrc, "RSS language configuration");
  mkdirSync(grammarOutDir, { recursive: true });
  copyFileTo(rssGrammarSrc, resolve(grammarOutDir, "rss.tmLanguage.json"), "synced RSS grammar");
  copyFileTo(
    rssConfigSrc,
    resolve(grammarOutDir, "rss.language-configuration.json"),
    "synced RSS language config"
  );
}

function buildWasm({ runtime }) {
  const args = ["build", "-p", "pd-vm-wasm"];
  if (runtime) {
    args.push("--features", "runtime");
  }
  args.push("--target", wasmTarget, "--release");
  run("cargo", args);
  ensureFileExists(compiledWasmPath, "compiled editor wasm");
}

function syncTargets(targets, runtime) {
  const targetsForBuild = targets.filter((target) => targetSpecs[target].needsRuntime === runtime);
  if (targetsForBuild.length === 0) {
    return;
  }

  buildWasm({ runtime });
  for (const target of targetsForBuild) {
    const spec = targetSpecs[target];
    copyFileTo(compiledWasmPath, spec.wasmOut, `copied ${target} wasm`);
    if (spec.grammarOutDir) {
      copyGrammarAssets(spec.grammarOutDir);
    }
  }
}

const targets = normalizeTargets(process.argv.slice(2));
run("rustup", ["target", "add", wasmTarget]);
syncTargets(targets, false);
syncTargets(targets, true);
