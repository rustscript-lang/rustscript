import "./style.css";

import * as monaco from "monaco-editor";
import editorWorker from "monaco-editor/esm/vs/editor/editor.worker?worker";
import jsonWorker from "monaco-editor/esm/vs/language/json/json.worker?worker";
import cssWorker from "monaco-editor/esm/vs/language/css/css.worker?worker";
import htmlWorker from "monaco-editor/esm/vs/language/html/html.worker?worker";
import tsWorker from "monaco-editor/esm/vs/language/typescript/ts.worker?worker";

import { ensureRustScriptLanguage } from "./monaco/rustscriptLanguage";
import { ensureSchemeLanguage } from "./monaco/schemeLanguage";
import {
  completionCatalogWithWasm,
  lintWithWasm,
  runWithWasm,
  type CompletionCatalog,
  type CompletionEntry,
  type LintDiagnostic,
  type SourceFlavor
} from "./wasmRuntime";

const MARKER_OWNER = "pd-vm-playground-lint";

const FLAVOR_OPTIONS: Array<{ value: SourceFlavor; label: string }> = [
  { value: "rustscript", label: "RustScript (.rss)" },
  { value: "javascript", label: "JavaScript (.js)" },
  { value: "lua", label: "Lua (.lua)" },
  { value: "scheme", label: "Scheme (.scm)" }
];

const SAMPLE_SOURCES: Record<SourceFlavor, string> = {
  rustscript: `
use stdlib::rss::strings as string;

use vm::{add_one};
use re;
use json;

// Complex RustScript example with closure capture, stdlib module use, and host calls.
let mut total = 0;
for (let mut i = 0; i < 4; i = i + 1) {
    total = total + i;
}

let total = if !string::non_empty("rustscript") => {
    let mut zeroed = 0;
    zeroed
} else => {
    let mut bumped = add_one(total);
    bumped
};

let mut base = 7;
let add = |value| value + base;
base = 8;
let mut closure_value = add(5);

let profile = { stats: { score: closure_value } };
let chained_score = profile?.stats?.score;
let missing_score = profile?.missing?.value;

let matched = match chained_score {
    12 => closure_value,
    _ => 0,
};

let regex_ok = re::match("^rustscript$", "RUSTSCRIPT", "i");
let payload = {
    lang: "rustscript",
    score: closure_value,
    matched: matched,
};
let payload_json = json::encode(payload);
let payload_decoded = json::decode(payload_json);
let json_score = payload_decoded.score;

if regex_ok && json_score == matched {
    print("closure_value is {:3}", closure_value);
} else {
    print(0);
}
`,
  javascript: ["let value = 21;", "console.log(value + 21);", "value + 21;"].join("\n"),
  lua: ["local value = 21", "print(value + 21)", "value + 21"].join("\n"),
  scheme: ["(define value 21)", "(print (+ value 21))", "(+ value 21)"].join("\n")
};

declare global {
  interface Window {
    MonacoEnvironment?: {
      getWorker(_: string, label: string): Worker;
    };
  }
}

window.MonacoEnvironment = {
  getWorker(_: string, label: string): Worker {
    if (label === "json") {
      return new jsonWorker();
    }
    if (label === "css" || label === "scss" || label === "less") {
      return new cssWorker();
    }
    if (label === "html" || label === "handlebars" || label === "razor") {
      return new htmlWorker();
    }
    if (label === "typescript" || label === "javascript") {
      return new tsWorker();
    }
    return new editorWorker();
  }
};

function languageForFlavor(flavor: SourceFlavor): string {
  if (flavor === "rustscript") {
    return "rustscript";
  }
  if (flavor === "javascript") {
    return "javascript";
  }
  if (flavor === "lua") {
    return "lua";
  }
  return "scheme";
}

function completionItemKind(kind: CompletionEntry["kind"]): monaco.languages.CompletionItemKind {
  if (kind === "function") {
    return monaco.languages.CompletionItemKind.Function;
  }
  if (kind === "module") {
    return monaco.languages.CompletionItemKind.Module;
  }
  return monaco.languages.CompletionItemKind.Snippet;
}

function registerCompletionProvider(
  languageId: string,
  entries: CompletionEntry[],
  triggerCharacters: string[]
): void {
  if (entries.length === 0) {
    return;
  }

  monaco.languages.registerCompletionItemProvider(languageId, {
    triggerCharacters,
    provideCompletionItems(model, position) {
      const word = model.getWordUntilPosition(position);
      const range: monaco.IRange = {
        startLineNumber: position.lineNumber,
        endLineNumber: position.lineNumber,
        startColumn: word.startColumn,
        endColumn: word.endColumn
      };

      const suggestions = entries.map((entry, index) => ({
        label: entry.label,
        kind: completionItemKind(entry.kind),
        insertText: entry.insertText,
        insertTextRules: monaco.languages.CompletionItemInsertTextRule.InsertAsSnippet,
        detail: entry.detail,
        documentation: entry.documentation,
        filterText: `${entry.label} ${entry.insertText}`,
        sortText: `${String(index).padStart(4, "0")}_${entry.label}`,
        range
      }));
      return { suggestions };
    }
  });
}

function registerCatalogCompletions(catalog: CompletionCatalog): void {
  registerCompletionProvider("rustscript", catalog.rustscript, [":", "!"]);
  registerCompletionProvider("javascript", catalog.javascript, ["."]);
  registerCompletionProvider("lua", catalog.lua, [".", ":"]);
  registerCompletionProvider("scheme", catalog.scheme, ["(", "."]);
}

function markerFromDiagnostic(
  diagnostic: LintDiagnostic,
  model: monaco.editor.ITextModel
): monaco.editor.IMarkerData {
  const maxLine = Math.max(1, model.getLineCount());
  const fallbackLine = Math.min(Math.max(diagnostic.line || 1, 1), maxLine);
  const rawRange = diagnostic.span
    ? {
        startLineNumber: diagnostic.span.startLine,
        startColumn: diagnostic.span.startColumn,
        endLineNumber: diagnostic.span.endLine,
        endColumn: diagnostic.span.endColumn
      }
    : {
        startLineNumber: fallbackLine,
        startColumn: 1,
        endLineNumber: fallbackLine,
        endColumn: Math.max(2, model.getLineMaxColumn(fallbackLine))
      };
  const range = model.validateRange(rawRange);
  return {
    severity: monaco.MarkerSeverity.Error,
    message: diagnostic.message,
    startLineNumber: range.startLineNumber,
    startColumn: range.startColumn,
    endLineNumber: range.endLineNumber,
    endColumn: range.endColumn
  };
}

function renderDiagnosticsList(container: HTMLElement, diagnostics: LintDiagnostic[]): void {
  if (diagnostics.length === 0) {
    container.textContent = "No lint diagnostics.";
    return;
  }

  const lines = diagnostics.map((diagnostic) => {
    const location =
      diagnostic.span !== null
        ? `${diagnostic.span.startLine}:${diagnostic.span.startColumn}`
        : `${Math.max(1, diagnostic.line)}:1`;
    return `[${location}] ${diagnostic.message}`;
  });
  container.textContent = lines.join("\n");
}

function setRunPanel(outputEl: HTMLElement, stackEl: HTMLElement, output: string[], stack: string[]): void {
  outputEl.textContent = output.length > 0 ? output.join("\n") : "<no print output>";
  stackEl.textContent = stack.length > 0 ? stack.join("\n") : "<empty stack>";
}

const app = document.querySelector<HTMLDivElement>("#app");
if (!app) {
  throw new Error("app root not found");
}

app.innerHTML = `
  <main class="page">
    <section class="hero">
      <h1>RustScript playground</h1>
      <p>Live lint markers come from wasm compiler diagnostics; Run executes source in wasm runtime and streams print output + final stack.</p>
    </section>
    <section class="workspace">
      <div class="toolbar">
        <div class="flavor-control flavor-control--hidden" aria-hidden="true">
          <label for="flavor-select">Flavor</label>
          <select id="flavor-select" aria-label="source flavor"></select>
        </div>
        <button id="run-button" type="button">Run</button>
        <span id="lint-status" class="status neutral">lint: idle</span>
        <span id="run-status" class="status neutral">run: idle</span>
      </div>
      <div class="editor-shell">
        <div id="editor" class="editor"></div>
      </div>
      <div class="panels">
        <article class="panel">
          <h2>Diagnostics</h2>
          <pre id="diagnostics" class="panel-content">No lint diagnostics.</pre>
        </article>
        <article class="panel">
          <h2>Print Output</h2>
          <pre id="run-output" class="panel-content">&lt;no print output&gt;</pre>
        </article>
        <article class="panel">
          <h2>Final Stack</h2>
          <pre id="run-stack" class="panel-content">&lt;empty stack&gt;</pre>
        </article>
      </div>
    </section>
  </main>
`;

const flavorSelect = document.querySelector<HTMLSelectElement>("#flavor-select");
const runButton = document.querySelector<HTMLButtonElement>("#run-button");
const lintStatus = document.querySelector<HTMLSpanElement>("#lint-status");
const runStatus = document.querySelector<HTMLSpanElement>("#run-status");
const diagnosticsEl = document.querySelector<HTMLElement>("#diagnostics");
const outputEl = document.querySelector<HTMLElement>("#run-output");
const stackEl = document.querySelector<HTMLElement>("#run-stack");
const editorHost = document.querySelector<HTMLElement>("#editor");

if (!flavorSelect || !runButton || !lintStatus || !runStatus || !diagnosticsEl || !outputEl || !stackEl || !editorHost) {
  throw new Error("playground UI nodes are missing");
}

const flavorSelectEl: HTMLSelectElement = flavorSelect;
const runButtonEl: HTMLButtonElement = runButton;
const lintStatusEl: HTMLSpanElement = lintStatus;
const runStatusEl: HTMLSpanElement = runStatus;
const diagnosticsPanelEl: HTMLElement = diagnosticsEl;
const outputPanelEl: HTMLElement = outputEl;
const stackPanelEl: HTMLElement = stackEl;
const editorHostEl: HTMLElement = editorHost;

for (const flavor of FLAVOR_OPTIONS) {
  const option = document.createElement("option");
  option.value = flavor.value;
  option.textContent = flavor.label;
  flavorSelectEl.append(option);
}

ensureRustScriptLanguage(monaco);
ensureSchemeLanguage(monaco);

void completionCatalogWithWasm()
  .then((catalog) => {
    registerCatalogCompletions(catalog);
  })
  .catch((error) => {
    const message = error instanceof Error ? error.message : "unknown completion catalog error";
    console.warn(`failed to load completion catalog: ${message}`);
  });

const models: Record<SourceFlavor, monaco.editor.ITextModel> = {
  rustscript: monaco.editor.createModel(SAMPLE_SOURCES.rustscript, languageForFlavor("rustscript")),
  javascript: monaco.editor.createModel(SAMPLE_SOURCES.javascript, languageForFlavor("javascript")),
  lua: monaco.editor.createModel(SAMPLE_SOURCES.lua, languageForFlavor("lua")),
  scheme: monaco.editor.createModel(SAMPLE_SOURCES.scheme, languageForFlavor("scheme"))
};

const editor = monaco.editor.create(editorHostEl, {
  model: models.rustscript,
  theme: "vs",
  minimap: { enabled: false },
  automaticLayout: true,
  fixedOverflowWidgets: true,
  wordWrap: "on",
  scrollBeyondLastLine: false,
  fontFamily: "\"IBM Plex Mono\", monospace",
  fontSize: 13,
  lineNumbersMinChars: 3
});

function refreshEditorGeometry(): void {
  monaco.editor.remeasureFonts();
  editor.layout();
}

refreshEditorGeometry();
if (typeof document !== "undefined" && "fonts" in document) {
  void document.fonts.ready.then(() => {
    refreshEditorGeometry();
  });
  document.fonts.addEventListener("loadingdone", () => {
    refreshEditorGeometry();
  });
}

let currentFlavor: SourceFlavor = "rustscript";
let lintSequence = 0;
let lintTimer: number | null = null;

function setStatus(node: HTMLElement, text: string, className: "neutral" | "ok" | "error" | "busy"): void {
  node.classList.remove("neutral", "ok", "error", "busy");
  node.classList.add(className);
  node.textContent = text;
}

function activeModel(): monaco.editor.ITextModel {
  const model = editor.getModel();
  if (!model) {
    throw new Error("editor model is missing");
  }
  return model;
}

async function runLintNow(): Promise<void> {
  const source = activeModel().getValue();
  const flavor = currentFlavor;
  lintSequence += 1;
  const seq = lintSequence;
  setStatus(lintStatusEl, "lint: running...", "busy");

  try {
    const report = await lintWithWasm(source, flavor);
    if (seq !== lintSequence) {
      return;
    }

    const model = activeModel();
    const markers = report.diagnostics.map((item) => markerFromDiagnostic(item, model));
    monaco.editor.setModelMarkers(model, MARKER_OWNER, markers);
    renderDiagnosticsList(diagnosticsPanelEl, report.diagnostics);
    if (report.diagnostics.length > 0) {
      setStatus(lintStatusEl, `lint: ${report.diagnostics.length} error(s)`, "error");
    } else {
      setStatus(lintStatusEl, "lint: ok", "ok");
    }
  } catch (error) {
    if (seq !== lintSequence) {
      return;
    }
    const message = error instanceof Error ? error.message : "lint failed";
    const model = activeModel();
    monaco.editor.setModelMarkers(model, MARKER_OWNER, [
      {
        severity: monaco.MarkerSeverity.Warning,
        message,
        startLineNumber: 1,
        startColumn: 1,
        endLineNumber: 1,
        endColumn: Math.max(2, model.getLineMaxColumn(1))
      }
    ]);
    diagnosticsPanelEl.textContent = message;
    setStatus(lintStatusEl, "lint: wasm error", "error");
  }
}

function scheduleLint(): void {
  if (lintTimer !== null) {
    window.clearTimeout(lintTimer);
  }
  lintTimer = window.setTimeout(() => {
    void runLintNow();
  }, 120);
}

for (const model of Object.values(models)) {
  model.onDidChangeContent(() => {
    if (editor.getModel() === model) {
      scheduleLint();
    }
  });
}

flavorSelectEl.value = currentFlavor;
flavorSelectEl.addEventListener("change", () => {
  const next = flavorSelectEl.value as SourceFlavor;
  if (!(next in models)) {
    return;
  }
  currentFlavor = next;
  editor.setModel(models[currentFlavor]);
  setStatus(runStatusEl, "run: idle", "neutral");
  scheduleLint();
});

runButtonEl.addEventListener("click", async () => {
  runButtonEl.disabled = true;
  setStatus(runStatusEl, "run: running...", "busy");

  const source = activeModel().getValue();
  try {
    const report = await runWithWasm(source, currentFlavor);
    const model = activeModel();
    const markers = report.diagnostics.map((item) => markerFromDiagnostic(item, model));
    monaco.editor.setModelMarkers(model, MARKER_OWNER, markers);
    renderDiagnosticsList(diagnosticsPanelEl, report.diagnostics);
    setRunPanel(outputPanelEl, stackPanelEl, report.output, report.stack);

    if (!report.ok || report.error) {
      const errorMessage = report.error ?? "runtime failed";
      setStatus(runStatusEl, `run: error (${errorMessage})`, "error");
    } else {
      setStatus(runStatusEl, "run: ok", "ok");
    }

    if (report.diagnostics.length > 0) {
      setStatus(lintStatusEl, `lint: ${report.diagnostics.length} error(s)`, "error");
    } else {
      setStatus(lintStatusEl, "lint: ok", "ok");
    }
  } catch (error) {
    const message = error instanceof Error ? error.message : "run failed";
    setStatus(runStatusEl, `run: wasm error (${message})`, "error");
  } finally {
    runButtonEl.disabled = false;
  }
});

scheduleLint();
