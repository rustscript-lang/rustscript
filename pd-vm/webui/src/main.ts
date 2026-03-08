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
  debugCommandWithWasm,
  lintWithWasm,
  runCommandWithWasm,
  runWithWasm,
  startDebugWithWasm,
  type CompletionCatalog,
  type CompletionEntry,
  type DebugCommandRequest,
  type DebugReport,
  type FuelConfig,
  type FuelState,
  type LintDiagnostic,
  type RunReport,
  type RunCommandRequest,
  type SourceFlavor
} from "./wasmRuntime";

const MARKER_OWNER = "pd-vm-playground-lint";
const BREAKPOINT_GLYPH_CLASS = "pd-debug-breakpoint-glyph";
const CURRENT_LINE_CLASS = "pd-debug-current-line";
const CURRENT_LINE_MARKER_CLASS = "pd-debug-current-line-marker";
const THEME_STORAGE_KEY = "pd-vm-webui-theme";
const FLAVOR_STORAGE_KEY = "pd-vm-webui-flavor";
const SOURCE_STORAGE_KEY_PREFIX = "pd-vm-webui-source:";
const FUEL_AMOUNT_STORAGE_KEY = "pd-vm-webui-fuel-amount";
const FUEL_INTERVAL_STORAGE_KEY = "pd-vm-webui-fuel-interval";
const DEFAULT_FUEL_HINT =
  "New runs and debug sessions start with the current amount and interval. Resume run adds the current amount before continuing.";

type ThemePreference = "light" | "dark" | "system";
type ResolvedTheme = "light" | "dark";

const FLAVOR_OPTIONS: Array<{ value: SourceFlavor; label: string }> = [
  { value: "rustscript", label: "RustScript (.rss)" },
  { value: "javascript", label: "JavaScript (.js)" },
  { value: "lua", label: "Lua (.lua)" },
  { value: "scheme", label: "Scheme (.scm)" }
];

const THEME_OPTIONS: Array<{ value: ThemePreference; label: string; icon: string; title: string }> = [
  { value: "system", label: "System", icon: "theme_system", title: "Follow system theme" },
  { value: "light", label: "Light", icon: "theme_light", title: "Light mode" },
  { value: "dark", label: "Dark", icon: "theme_dark", title: "Dark mode" }
];

const SAMPLE_SOURCES: Record<SourceFlavor, string> = {
  rustscript: `
use stdlib::rss::strings as string;

use re;
use json;
use runtime;

// Complex RustScript example with closure capture, stdlib module use, and host calls.
let mut total = 0;
for (let mut i = 0; i < 4; i = i + 1) {
    total = total + i;
}

runtime::sleep(100);

let total = if !string::non_empty("rustscript") => {
    let zeroed = 0;
    zeroed
} else => {
    let bumped = total + 1;
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

function iconSvg(content: string): string {
  return `<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${content}</svg>`;
}

const ICONS: Record<string, string> = {
  run: iconSvg('<polygon points="8 5 19 12 8 19 8 5" fill="currentColor" stroke="none"></polygon>'),
  debug: iconSvg(
    '<path d="M9 9h6"></path><path d="M9 15h6"></path><path d="M10 5.5 8.5 3.5"></path><path d="M14 5.5 15.5 3.5"></path><rect x="7" y="7" width="10" height="12" rx="4"></rect><path d="M4 9h3"></path><path d="M17 9h3"></path><path d="M3 13h4"></path><path d="M17 13h4"></path>'
  ),
  theme_system: iconSvg(
    '<rect x="3" y="5" width="18" height="12" rx="2"></rect><path d="M8 21h8"></path><path d="M12 17v4"></path>'
  ),
  theme_light: iconSvg(
    '<circle cx="12" cy="12" r="4"></circle><path d="M12 2v2.5"></path><path d="M12 19.5V22"></path><path d="m4.93 4.93 1.77 1.77"></path><path d="m17.3 17.3 1.77 1.77"></path><path d="M2 12h2.5"></path><path d="M19.5 12H22"></path><path d="m4.93 19.07 1.77-1.77"></path><path d="m17.3 6.7 1.77-1.77"></path>'
  ),
  theme_dark: iconSvg('<path d="M20 14.5A8.5 8.5 0 1 1 9.5 4 6.8 6.8 0 0 0 20 14.5"></path>'),
  diagnostics: iconSvg(
    '<path d="M12 3 21 19H3L12 3"></path><path d="M12 9v4"></path><circle cx="12" cy="16" r="1"></circle>'
  ),
  output: iconSvg(
    '<rect x="3" y="5" width="18" height="14" rx="2"></rect><path d="m7 10 3 2-3 2"></path><path d="M13 14h4"></path>'
  ),
  fuel: iconSvg(
    '<path d="M12 3c2.7 3.3 5 5.7 5 9a5 5 0 1 1-10 0c0-3.3 2.3-5.7 5-9"></path><path d="M10 14c.5 1 1.3 1.8 2.5 2.2"></path>'
  ),
  where: iconSvg(
    '<circle cx="12" cy="12" r="9"></circle><line x1="12" y1="3" x2="12" y2="7"></line><line x1="12" y1="17" x2="12" y2="21"></line><line x1="3" y1="12" x2="7" y2="12"></line><line x1="17" y1="12" x2="21" y2="12"></line>'
  ),
  locals: iconSvg(
    '<line x1="8" y1="6" x2="21" y2="6"></line><line x1="8" y1="12" x2="21" y2="12"></line><line x1="8" y1="18" x2="21" y2="18"></line><circle cx="4" cy="6" r="1.2"></circle><circle cx="4" cy="12" r="1.2"></circle><circle cx="4" cy="18" r="1.2"></circle>'
  ),
  stack: iconSvg(
    '<polygon points="12 2 2 7 12 12 22 7 12 2"></polygon><polyline points="2 12 12 17 22 12"></polyline><polyline points="2 17 12 22 22 17"></polyline>'
  ),
  step: iconSvg('<line x1="5" y1="12" x2="19" y2="12"></line><polyline points="12 5 19 12 12 19"></polyline>'),
  next: iconSvg('<polyline points="7 17 12 12 7 7"></polyline><polyline points="13 17 18 12 13 7"></polyline>'),
  out: iconSvg('<polyline points="9 14 4 9 9 4"></polyline><path d="M20 20v-7a4 4 0 0 0-4-4H4"></path>'),
  continue: iconSvg('<polygon points="7 4 20 12 7 20 7 4" fill="currentColor" stroke="none"></polygon>'),
  stop: iconSvg('<rect x="5" y="5" width="14" height="14" rx="2" fill="currentColor" stroke="none"></rect>')
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

function isThemePreference(value: string): value is ThemePreference {
  return value === "light" || value === "dark" || value === "system";
}

function loadThemePreference(): ThemePreference {
  try {
    const stored = window.localStorage.getItem(THEME_STORAGE_KEY);
    if (stored && isThemePreference(stored)) {
      return stored;
    }
  } catch {
    // Ignore storage failures and fall back to the system preference.
  }
  return "system";
}

function isSourceFlavor(value: string): value is SourceFlavor {
  return FLAVOR_OPTIONS.some((option) => option.value === value);
}

function sourceStorageKey(flavor: SourceFlavor): string {
  return `${SOURCE_STORAGE_KEY_PREFIX}${flavor}`;
}

function loadCurrentFlavor(): SourceFlavor {
  try {
    const stored = window.localStorage.getItem(FLAVOR_STORAGE_KEY);
    if (stored && isSourceFlavor(stored)) {
      return stored;
    }
  } catch {
    // Ignore storage failures and fall back to the default flavor.
  }
  return "rustscript";
}

function loadSourceForFlavor(flavor: SourceFlavor): string {
  try {
    const stored = window.localStorage.getItem(sourceStorageKey(flavor));
    if (stored !== null) {
      return stored;
    }
  } catch {
    // Ignore storage failures and fall back to the bundled sample.
  }
  return SAMPLE_SOURCES[flavor];
}

function loadFuelAmountInput(): string {
  try {
    return window.localStorage.getItem(FUEL_AMOUNT_STORAGE_KEY) ?? "";
  } catch {
    return "";
  }
}

function loadFuelIntervalInput(): string {
  try {
    return window.localStorage.getItem(FUEL_INTERVAL_STORAGE_KEY) ?? "1";
  } catch {
    return "1";
  }
}

function resolveTheme(preference: ThemePreference, query: MediaQueryList | null): ResolvedTheme {
  if (preference === "system") {
    return query?.matches ? "dark" : "light";
  }
  return preference;
}

function applyDocumentTheme(theme: ResolvedTheme): void {
  document.documentElement.dataset.theme = theme;
  document.documentElement.style.colorScheme = theme;
}

function monacoThemeName(theme: ResolvedTheme): "vs" | "vs-dark" {
  return theme === "dark" ? "vs-dark" : "vs";
}

function looksLikeIdentifier(value: string): boolean {
  return /^[A-Za-z_][A-Za-z0-9_]*$/.test(value);
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

function mountIconButton(button: HTMLButtonElement, icon: string, label: string): void {
  button.innerHTML = `${ICONS[icon] ?? ""}<span class="sr-only">${label}</span>`;
}

function parseHoverValue(variable: string, output: string): string | null {
  const trimmed = output.trim();
  if (!trimmed) {
    return null;
  }
  const prefix = `${variable} = `;
  if (trimmed.startsWith(prefix)) {
    return trimmed.slice(prefix.length);
  }
  if (
    trimmed === "no debug info" ||
    trimmed.startsWith("unknown local ") ||
    trimmed.startsWith("local '")
  ) {
    return null;
  }
  return trimmed;
}

function panelTitle(icon: string, label: string): string {
  return `<span class="panel-title">${ICONS[icon] ?? ""}<span>${label}</span></span>`;
}

const systemThemeQuery =
  typeof window !== "undefined" && typeof window.matchMedia === "function"
    ? window.matchMedia("(prefers-color-scheme: dark)")
    : null;
const initialFlavor = loadCurrentFlavor();
const initialFuelAmount = loadFuelAmountInput();
const initialFuelInterval = loadFuelIntervalInput();
const initialThemePreference = loadThemePreference();
const initialResolvedTheme = resolveTheme(initialThemePreference, systemThemeQuery);
applyDocumentTheme(initialResolvedTheme);

const app = document.querySelector<HTMLDivElement>("#app");
if (!app) {
  throw new Error("app root not found");
}

app.innerHTML = `
  <main class="page">
    <section class="hero">
      <h1>RustScript playground</h1>
      <p>Run or debug directly in wasm runtime with Monaco breakpoints, stepping controls, and hover variable inspect.</p>
      <p><a href="./about.html" style="color: #58a6ff; text-decoration: underline;">Read more about the VM & RustScript here.</a></p>
    </section>
    <section class="workspace">
      <div class="toolbar">
        <div class="flavor-control flavor-control--hidden" aria-hidden="true">
          <label for="flavor-select">Flavor</label>
          <select id="flavor-select" aria-label="source flavor"></select>
        </div>
        <button id="run-button" class="toolbar-action" type="button" title="Run" aria-label="Run"></button>
        <button id="debug-start-button" class="toolbar-action" type="button" title="Debug" aria-label="Debug"></button>
        <div class="debug-toolbar" role="toolbar" aria-label="debug controls">
          <button id="debug-where-button" class="icon-button icon-button--outline" type="button" title="Where" aria-label="Where"></button>
          <button id="debug-locals-button" class="icon-button icon-button--outline" type="button" title="Locals" aria-label="Locals"></button>
          <button id="debug-stack-button" class="icon-button icon-button--outline" type="button" title="Stack" aria-label="Stack"></button>
          <span class="toolbar-sep" aria-hidden="true"></span>
          <button id="debug-step-button" class="icon-button" type="button" title="Step" aria-label="Step"></button>
          <button id="debug-next-button" class="icon-button" type="button" title="Next" aria-label="Next"></button>
          <button id="debug-out-button" class="icon-button" type="button" title="Out" aria-label="Out"></button>
          <button id="debug-continue-button" class="icon-button" type="button" title="Continue" aria-label="Continue"></button>
          <span class="toolbar-sep" aria-hidden="true"></span>
          <button id="debug-stop-button" class="icon-button icon-button--stop" type="button" title="Stop" aria-label="Stop"></button>
        </div>
        <span id="lint-status" class="status neutral">lint: idle</span>
        <span id="run-status" class="status neutral">run: idle</span>
        <span id="debug-status" class="status neutral">debug: idle</span>
        <div id="theme-control" class="theme-control" role="group" aria-label="theme mode" data-theme="system">
          <button id="theme-system-button" class="theme-option" type="button" title="Follow system theme" aria-label="Follow system theme"></button>
          <button id="theme-light-button" class="theme-option" type="button" title="Light mode" aria-label="Light mode"></button>
          <button id="theme-dark-button" class="theme-option" type="button" title="Dark mode" aria-label="Dark mode"></button>
        </div>
      </div>
      <div class="workspace-body">
        <div class="editor-shell">
          <div id="editor" class="editor"></div>
        </div>
        <aside class="panels" aria-label="runtime details">
          <article class="panel">
            <h2>${panelTitle("diagnostics", "Diagnostics")}</h2>
            <pre id="diagnostics" class="panel-content">No lint diagnostics.</pre>
          </article>
          <article class="panel">
            <h2>${panelTitle("output", "Print Output")}</h2>
            <pre id="run-output" class="panel-content">&lt;no print output&gt;</pre>
          </article>
          <article class="panel">
            <h2>${panelTitle("stack", "Final Stack")}</h2>
            <pre id="run-stack" class="panel-content">&lt;empty stack&gt;</pre>
          </article>
          <article class="panel">
            <h2>${panelTitle("debug", "Debugger")}</h2>
            <pre id="debug-output" class="panel-content">&lt;no debugger output&gt;</pre>
            <div id="debug-hover" class="debug-hover">hover inspect: (none)</div>
          </article>
          <article class="panel panel--fuel">
            <h2>${panelTitle("fuel", "Fuel")}</h2>
            <div class="fuel-panel">
              <label class="fuel-field" for="fuel-amount-input">
                <span>Fuel Amount</span>
                <input
                  id="fuel-amount-input"
                  class="fuel-input"
                  type="number"
                  min="0"
                  step="1"
                  inputmode="numeric"
                  placeholder="disabled"
                />
              </label>
              <label class="fuel-field" for="fuel-interval-input">
                <span>Check Interval</span>
                <input
                  id="fuel-interval-input"
                  class="fuel-input"
                  type="number"
                  min="1"
                  step="1"
                  inputmode="numeric"
                />
              </label>
              <div class="fuel-actions">
                <button id="debug-fuel-set-button" class="panel-button" type="button">Set Debug Fuel</button>
                <button id="debug-fuel-add-button" class="panel-button panel-button--secondary" type="button">Add Debug Fuel</button>
                <button id="debug-fuel-interval-button" class="panel-button panel-button--secondary" type="button">Apply Debug Interval</button>
                <button id="run-resume-button" class="panel-button" type="button">Resume Run</button>
              </div>
              <div id="fuel-hint" class="fuel-hint">${DEFAULT_FUEL_HINT}</div>
              <div class="fuel-state-list">
                <div id="run-fuel-state" class="fuel-state-line">Run session: idle</div>
                <div id="debug-fuel-state" class="fuel-state-line">Debugger fuel: idle</div>
              </div>
            </div>
          </article>
        </aside>
      </div>
    </section>
  </main>
`;

const flavorSelect = document.querySelector<HTMLSelectElement>("#flavor-select");
const themeControl = document.querySelector<HTMLElement>("#theme-control");
const themeSystemButton = document.querySelector<HTMLButtonElement>("#theme-system-button");
const themeLightButton = document.querySelector<HTMLButtonElement>("#theme-light-button");
const themeDarkButton = document.querySelector<HTMLButtonElement>("#theme-dark-button");
const runButton = document.querySelector<HTMLButtonElement>("#run-button");
const debugStartButton = document.querySelector<HTMLButtonElement>("#debug-start-button");
const debugWhereButton = document.querySelector<HTMLButtonElement>("#debug-where-button");
const debugLocalsButton = document.querySelector<HTMLButtonElement>("#debug-locals-button");
const debugStackButton = document.querySelector<HTMLButtonElement>("#debug-stack-button");
const debugStepButton = document.querySelector<HTMLButtonElement>("#debug-step-button");
const debugNextButton = document.querySelector<HTMLButtonElement>("#debug-next-button");
const debugOutButton = document.querySelector<HTMLButtonElement>("#debug-out-button");
const debugContinueButton = document.querySelector<HTMLButtonElement>("#debug-continue-button");
const debugStopButton = document.querySelector<HTMLButtonElement>("#debug-stop-button");
const lintStatus = document.querySelector<HTMLSpanElement>("#lint-status");
const runStatus = document.querySelector<HTMLSpanElement>("#run-status");
const debugStatus = document.querySelector<HTMLSpanElement>("#debug-status");
const diagnosticsEl = document.querySelector<HTMLElement>("#diagnostics");
const outputEl = document.querySelector<HTMLElement>("#run-output");
const stackEl = document.querySelector<HTMLElement>("#run-stack");
const debugOutputEl = document.querySelector<HTMLElement>("#debug-output");
const debugHoverEl = document.querySelector<HTMLElement>("#debug-hover");
const fuelAmountInput = document.querySelector<HTMLInputElement>("#fuel-amount-input");
const fuelIntervalInput = document.querySelector<HTMLInputElement>("#fuel-interval-input");
const debugFuelSetButton = document.querySelector<HTMLButtonElement>("#debug-fuel-set-button");
const debugFuelAddButton = document.querySelector<HTMLButtonElement>("#debug-fuel-add-button");
const debugFuelIntervalButton = document.querySelector<HTMLButtonElement>("#debug-fuel-interval-button");
const runResumeButton = document.querySelector<HTMLButtonElement>("#run-resume-button");
const fuelHintEl = document.querySelector<HTMLElement>("#fuel-hint");
const runFuelStateEl = document.querySelector<HTMLElement>("#run-fuel-state");
const debugFuelStateEl = document.querySelector<HTMLElement>("#debug-fuel-state");
const editorHost = document.querySelector<HTMLElement>("#editor");

if (
  !flavorSelect ||
  !themeControl ||
  !themeSystemButton ||
  !themeLightButton ||
  !themeDarkButton ||
  !runButton ||
  !debugStartButton ||
  !debugWhereButton ||
  !debugLocalsButton ||
  !debugStackButton ||
  !debugStepButton ||
  !debugNextButton ||
  !debugOutButton ||
  !debugContinueButton ||
  !debugStopButton ||
  !lintStatus ||
  !runStatus ||
  !debugStatus ||
  !diagnosticsEl ||
  !outputEl ||
  !stackEl ||
  !debugOutputEl ||
  !debugHoverEl ||
  !fuelAmountInput ||
  !fuelIntervalInput ||
  !debugFuelSetButton ||
  !debugFuelAddButton ||
  !debugFuelIntervalButton ||
  !runResumeButton ||
  !fuelHintEl ||
  !runFuelStateEl ||
  !debugFuelStateEl ||
  !editorHost
) {
  throw new Error("playground UI nodes are missing");
}

mountIconButton(runButton, "run", "Run");
mountIconButton(debugStartButton, "debug", "Debug");
mountIconButton(themeSystemButton, "theme_system", "Follow system theme");
mountIconButton(themeLightButton, "theme_light", "Light mode");
mountIconButton(themeDarkButton, "theme_dark", "Dark mode");
mountIconButton(debugWhereButton, "where", "Where");
mountIconButton(debugLocalsButton, "locals", "Locals");
mountIconButton(debugStackButton, "stack", "Stack");
mountIconButton(debugStepButton, "step", "Step");
mountIconButton(debugNextButton, "next", "Next");
mountIconButton(debugOutButton, "out", "Out");
mountIconButton(debugContinueButton, "continue", "Continue");
mountIconButton(debugStopButton, "stop", "Stop");

const flavorSelectEl: HTMLSelectElement = flavorSelect;
const themeControlEl: HTMLElement = themeControl;
const themeSystemButtonEl: HTMLButtonElement = themeSystemButton;
const themeLightButtonEl: HTMLButtonElement = themeLightButton;
const themeDarkButtonEl: HTMLButtonElement = themeDarkButton;
const runButtonEl: HTMLButtonElement = runButton;
const debugStartButtonEl: HTMLButtonElement = debugStartButton;
const debugWhereButtonEl: HTMLButtonElement = debugWhereButton;
const debugLocalsButtonEl: HTMLButtonElement = debugLocalsButton;
const debugStackButtonEl: HTMLButtonElement = debugStackButton;
const debugStepButtonEl: HTMLButtonElement = debugStepButton;
const debugNextButtonEl: HTMLButtonElement = debugNextButton;
const debugOutButtonEl: HTMLButtonElement = debugOutButton;
const debugContinueButtonEl: HTMLButtonElement = debugContinueButton;
const debugStopButtonEl: HTMLButtonElement = debugStopButton;
const lintStatusEl: HTMLSpanElement = lintStatus;
const runStatusEl: HTMLSpanElement = runStatus;
const debugStatusEl: HTMLSpanElement = debugStatus;
const diagnosticsPanelEl: HTMLElement = diagnosticsEl;
const outputPanelEl: HTMLElement = outputEl;
const stackPanelEl: HTMLElement = stackEl;
const debugOutputPanelEl: HTMLElement = debugOutputEl;
const debugHoverPanelEl: HTMLElement = debugHoverEl;
const fuelAmountInputEl: HTMLInputElement = fuelAmountInput;
const fuelIntervalInputEl: HTMLInputElement = fuelIntervalInput;
const debugFuelSetButtonEl: HTMLButtonElement = debugFuelSetButton;
const debugFuelAddButtonEl: HTMLButtonElement = debugFuelAddButton;
const debugFuelIntervalButtonEl: HTMLButtonElement = debugFuelIntervalButton;
const runResumeButtonEl: HTMLButtonElement = runResumeButton;
const fuelHintPanelEl: HTMLElement = fuelHintEl;
const runFuelStatePanelEl: HTMLElement = runFuelStateEl;
const debugFuelStatePanelEl: HTMLElement = debugFuelStateEl;
const editorHostEl: HTMLElement = editorHost;

fuelAmountInputEl.value = initialFuelAmount;
fuelIntervalInputEl.value = initialFuelInterval;

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
  rustscript: monaco.editor.createModel(loadSourceForFlavor("rustscript"), languageForFlavor("rustscript")),
  javascript: monaco.editor.createModel(loadSourceForFlavor("javascript"), languageForFlavor("javascript")),
  lua: monaco.editor.createModel(loadSourceForFlavor("lua"), languageForFlavor("lua")),
  scheme: monaco.editor.createModel(loadSourceForFlavor("scheme"), languageForFlavor("scheme"))
};

const lineBreakpointsByFlavor: Record<SourceFlavor, Set<number>> = {
  rustscript: new Set<number>(),
  javascript: new Set<number>(),
  lua: new Set<number>(),
  scheme: new Set<number>()
};

const editor = monaco.editor.create(editorHostEl, {
  model: models[initialFlavor],
  theme: monacoThemeName(initialResolvedTheme),
  minimap: { enabled: false },
  automaticLayout: true,
  fixedOverflowWidgets: true,
  wordWrap: "on",
  scrollBeyondLastLine: false,
  glyphMargin: true,
  lineDecorationsWidth: 20,
  fontFamily: "\"IBM Plex Mono\", monospace",
  fontSize: 13,
  lineNumbersMinChars: 3,
  renderLineHighlight: "none",
  hover: {
    enabled: true,
    delay: 220,
    sticky: true
  }
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

let currentFlavor: SourceFlavor = initialFlavor;
let themePreference: ThemePreference = initialThemePreference;
let resolvedTheme: ResolvedTheme = initialResolvedTheme;
let lintSequence = 0;
let lintTimer: number | null = null;
const sourcePersistTimers = new Map<SourceFlavor, number>();
let runBusy = false;
let runSessionActive = false;
let runSessionYielded = false;
let runFuelState: FuelState = {
  enabled: false,
  remaining: null,
  checkInterval: 1
};
let runSessionMessage = "Run session: idle";
let debugBusy = false;
let debugSessionActive = false;
let debugCurrentLine: number | null = null;
let debugFuelState: FuelState = {
  enabled: false,
  remaining: null,
  checkInterval: 1
};
let debugHoveredVar = "";
let debugHoverActiveKey = "";
let debugDecorationIds: string[] = [];
const debugHoverCache = new Map<string, string | null>();
const debugHoverInflight = new Map<string, Promise<string | null>>();
const themeButtons: Record<ThemePreference, HTMLButtonElement> = {
  system: themeSystemButtonEl,
  light: themeLightButtonEl,
  dark: themeDarkButtonEl
};

function setStatus(node: HTMLElement, text: string, className: "neutral" | "ok" | "error" | "busy"): void {
  node.classList.remove("neutral", "ok", "error", "busy");
  node.classList.add(className);
  node.textContent = text;
}

function persistThemePreference(preference: ThemePreference): void {
  try {
    window.localStorage.setItem(THEME_STORAGE_KEY, preference);
  } catch {
    // Ignore storage failures; the current session theme still applies.
  }
}

function persistCurrentFlavor(flavor: SourceFlavor): void {
  try {
    window.localStorage.setItem(FLAVOR_STORAGE_KEY, flavor);
  } catch {
    // Ignore storage failures; the current session still works.
  }
}

function persistSourceForFlavor(flavor: SourceFlavor, value: string): void {
  try {
    window.localStorage.setItem(sourceStorageKey(flavor), value);
  } catch {
    // Ignore storage failures; the current session still works.
  }
}

function persistFuelAmount(value: string): void {
  try {
    window.localStorage.setItem(FUEL_AMOUNT_STORAGE_KEY, value);
  } catch {
    // Ignore storage failures; the current session still works.
  }
}

function persistFuelInterval(value: string): void {
  try {
    window.localStorage.setItem(FUEL_INTERVAL_STORAGE_KEY, value);
  } catch {
    // Ignore storage failures; the current session still works.
  }
}

function scheduleSourcePersist(flavor: SourceFlavor): void {
  const pendingTimer = sourcePersistTimers.get(flavor);
  if (pendingTimer !== undefined) {
    window.clearTimeout(pendingTimer);
  }

  const nextTimer = window.setTimeout(() => {
    sourcePersistTimers.delete(flavor);
    persistSourceForFlavor(flavor, models[flavor].getValue());
  }, 180);
  sourcePersistTimers.set(flavor, nextTimer);
}

function flushPersistedSources(): void {
  for (const timer of sourcePersistTimers.values()) {
    window.clearTimeout(timer);
  }
  sourcePersistTimers.clear();

  for (const flavor of FLAVOR_OPTIONS) {
    persistSourceForFlavor(flavor.value, models[flavor.value].getValue());
  }
}

function defaultFuelState(): FuelState {
  return {
    enabled: false,
    remaining: null,
    checkInterval: 1
  };
}

function formatFuelState(fuel: FuelState): string {
  if (!fuel.enabled) {
    return `disabled (interval ${fuel.checkInterval})`;
  }
  return `${fuel.remaining ?? 0} left (interval ${fuel.checkInterval})`;
}

function setFuelHint(message: string | null): void {
  fuelHintPanelEl.textContent = message ?? DEFAULT_FUEL_HINT;
}

function syncFuelPanel(): void {
  runFuelStatePanelEl.textContent = runSessionActive
    ? `Run session: ${runSessionMessage} | ${formatFuelState(runFuelState)}`
    : "Run session: idle";
  debugFuelStatePanelEl.textContent = debugSessionActive
    ? `Debugger fuel: ${formatFuelState(debugFuelState)}`
    : "Debugger fuel: idle";
  runResumeButtonEl.disabled = !runSessionActive || runBusy;
  debugFuelSetButtonEl.disabled = !debugSessionActive || debugBusy;
  debugFuelAddButtonEl.disabled = !debugSessionActive || debugBusy;
  debugFuelIntervalButtonEl.disabled = !debugSessionActive || debugBusy;
}

type ParsedFuelForm = {
  amount: number | null;
  interval: number;
};

function readFuelForm(): ParsedFuelForm | null {
  const rawAmount = fuelAmountInputEl.value.trim();
  const rawInterval = fuelIntervalInputEl.value.trim();

  let amount: number | null = null;
  if (rawAmount.length > 0) {
    const parsedAmount = Number(rawAmount);
    if (!Number.isSafeInteger(parsedAmount) || parsedAmount < 0) {
      setFuelHint("Fuel amount must be a non-negative integer.");
      return null;
    }
    amount = parsedAmount;
  }

  const parsedInterval = Number(rawInterval);
  if (!Number.isSafeInteger(parsedInterval) || parsedInterval < 1) {
    setFuelHint("Fuel check interval must be an integer greater than or equal to 1.");
    return null;
  }

  setFuelHint(runSessionYielded ? "Run paused. Enter more fuel and click Resume Run." : null);
  return {
    amount,
    interval: parsedInterval
  };
}

function currentFuelConfig(): FuelConfig | null {
  const parsed = readFuelForm();
  if (!parsed) {
    return null;
  }
  return {
    fuel: parsed.amount,
    fuelCheckInterval: parsed.interval
  };
}

function syncThemeButtons(preference: ThemePreference): void {
  themeControlEl.dataset.theme = preference;
  for (const option of THEME_OPTIONS) {
    const button = themeButtons[option.value];
    const isActive = option.value === preference;
    button.classList.toggle("is-active", isActive);
    button.setAttribute("aria-pressed", isActive ? "true" : "false");
    button.title = option.title;
  }
}

function applyThemePreference(preference: ThemePreference, persist: boolean): void {
  themePreference = preference;
  resolvedTheme = resolveTheme(themePreference, systemThemeQuery);
  applyDocumentTheme(resolvedTheme);
  monaco.editor.setTheme(monacoThemeName(resolvedTheme));
  syncThemeButtons(themePreference);
  if (persist) {
    persistThemePreference(themePreference);
  }
}

function activeModel(): monaco.editor.ITextModel {
  const model = editor.getModel();
  if (!model) {
    throw new Error("editor model is missing");
  }
  return model;
}

function activeBreakpoints(): Set<number> {
  return lineBreakpointsByFlavor[currentFlavor];
}

function clearHoverInspect(): void {
  debugHoveredVar = "";
  debugHoverActiveKey = "";
  debugHoverPanelEl.textContent = "hover inspect: (none)";
}

function applyDebugControlState(): void {
  const hasSession = debugSessionActive;
  const disableCommands = !hasSession || debugBusy;

  debugStartButtonEl.disabled = debugBusy;
  debugWhereButtonEl.disabled = disableCommands;
  debugLocalsButtonEl.disabled = disableCommands;
  debugStackButtonEl.disabled = disableCommands;
  debugStepButtonEl.disabled = disableCommands;
  debugNextButtonEl.disabled = disableCommands;
  debugOutButtonEl.disabled = disableCommands;
  debugContinueButtonEl.disabled = disableCommands;
  debugStopButtonEl.disabled = !hasSession || debugBusy;
  syncFuelPanel();
}

function applyRunControlState(): void {
  runButtonEl.disabled = runBusy;
  syncFuelPanel();
}

function applyDebugDecorations(): void {
  const decorations: monaco.editor.IModelDeltaDecoration[] = [];
  if (debugSessionActive && debugCurrentLine && debugCurrentLine > 0) {
    decorations.push({
      range: new monaco.Range(debugCurrentLine, 1, debugCurrentLine, 1),
      options: {
        isWholeLine: true,
        className: CURRENT_LINE_CLASS,
        linesDecorationsClassName: CURRENT_LINE_MARKER_CLASS
      }
    });
  }

  const sortedBreakpoints = [...activeBreakpoints()].sort((lhs, rhs) => lhs - rhs);
  for (const line of sortedBreakpoints) {
    decorations.push({
      range: new monaco.Range(line, 1, line, 1),
      options: {
        isWholeLine: true,
        glyphMarginClassName: BREAKPOINT_GLYPH_CLASS,
        glyphMarginHoverMessage: { value: "Breakpoint" }
      }
    });
  }

  debugDecorationIds = editor.deltaDecorations(debugDecorationIds, decorations);
}

type ApplyDebugReportOptions = {
  syncBreakpoints?: boolean;
  showCommandOutput?: boolean;
  updateStatus?: boolean;
};

function applyDebugReport(report: DebugReport, options: ApplyDebugReportOptions = {}): void {
  const syncBreakpoints = options.syncBreakpoints ?? true;
  const showCommandOutput = options.showCommandOutput ?? true;
  const updateStatus = options.updateStatus ?? true;

  if (syncBreakpoints) {
    const breakpoints = activeBreakpoints();
    breakpoints.clear();
    for (const line of report.breakpoints) {
      if (line > 0) {
        breakpoints.add(line);
      }
    }
  }

  debugCurrentLine = report.currentLine;
  debugFuelState = report.fuel;
  applyDebugDecorations();
  syncFuelPanel();

  const model = activeModel();
  const markers = report.diagnostics.map((item) => markerFromDiagnostic(item, model));
  monaco.editor.setModelMarkers(model, MARKER_OWNER, markers);
  renderDiagnosticsList(diagnosticsPanelEl, report.diagnostics);

  if (report.diagnostics.length > 0) {
    setStatus(lintStatusEl, `lint: ${report.diagnostics.length} error(s)`, "error");
  } else {
    setStatus(lintStatusEl, "lint: ok", "ok");
  }

  setRunPanel(outputPanelEl, stackPanelEl, report.output, report.stack);

  if (showCommandOutput) {
    const chunks: string[] = [];
    const command = report.commandOutput.trim();
    if (command.length > 0) {
      chunks.push(command);
    }
    if (report.error) {
      chunks.push(`error: ${report.error}`);
    }
    debugOutputPanelEl.textContent = chunks.length > 0 ? chunks.join("\n") : "<no debugger output>";
  }

  if (!updateStatus) {
    return;
  }
  if (report.error) {
    setStatus(debugStatusEl, `debug: error (${report.error})`, "error");
    return;
  }
  if (report.currentLine) {
    setStatus(debugStatusEl, `debug: paused @ ${report.currentLine}`, "ok");
    return;
  }
  if (report.halted) {
    setStatus(debugStatusEl, "debug: halted", "ok");
    return;
  }
  setStatus(debugStatusEl, "debug: attached", "ok");
}

function applyInactiveDebugState(message: string): void {
  debugSessionActive = false;
  debugCurrentLine = null;
  debugBusy = false;
  debugFuelState = defaultFuelState();
  applyDebugControlState();
  applyDebugDecorations();
  clearHoverInspect();
  if (message) {
    debugOutputPanelEl.textContent = message;
  }
  setStatus(debugStatusEl, "debug: idle", "neutral");
}

function applyInactiveRunState(): void {
  runBusy = false;
  runSessionActive = false;
  runSessionYielded = false;
  runFuelState = defaultFuelState();
  runSessionMessage = "Run session: idle";
  setFuelHint(null);
  applyRunControlState();
}

function applyRunReport(report: RunReport): void {
  const model = activeModel();
  const markers = report.diagnostics.map((item) => markerFromDiagnostic(item, model));
  monaco.editor.setModelMarkers(model, MARKER_OWNER, markers);
  renderDiagnosticsList(diagnosticsPanelEl, report.diagnostics);
  setRunPanel(outputPanelEl, stackPanelEl, report.output, report.stack);

  if (report.diagnostics.length > 0) {
    setStatus(lintStatusEl, `lint: ${report.diagnostics.length} error(s)`, "error");
  } else {
    setStatus(lintStatusEl, "lint: ok", "ok");
  }

  runFuelState = report.fuel;
  runSessionActive = !report.halted && report.error === null;
  if (!runSessionActive) {
    runSessionYielded = false;
  } else if (report.yielded) {
    runSessionYielded = true;
  }

  const commandOutput = report.commandOutput.trim();
  if (commandOutput.length > 0) {
    runSessionMessage = commandOutput;
  } else if (runSessionActive) {
    runSessionMessage = runSessionYielded ? "paused" : "active";
  } else if (report.halted) {
    runSessionMessage = "program halted";
  } else {
    runSessionMessage = "idle";
  }

  if (report.error) {
    setStatus(runStatusEl, `run: error (${report.error})`, "error");
  } else if (report.halted) {
    setStatus(runStatusEl, "run: ok", "ok");
  } else if (runSessionYielded) {
    setStatus(runStatusEl, "run: yielded", "busy");
  } else {
    setStatus(runStatusEl, "run: paused", "busy");
  }

  setFuelHint(runSessionYielded ? "Run paused. Enter more fuel and click Resume Run." : null);
  syncFuelPanel();
}

async function sendRunCommand(command: RunCommandRequest): Promise<RunReport | null> {
  if (!runSessionActive && command.kind !== "stop") {
    return null;
  }

  runBusy = true;
  applyRunControlState();
  setStatus(runStatusEl, `run: ${command.kind}...`, "busy");

  try {
    const report = await runCommandWithWasm(command);
    if (command.kind === "stop" || report.error === "run session is not active") {
      applyInactiveRunState();
      if (report.error) {
        setStatus(runStatusEl, `run: error (${report.error})`, "error");
      } else {
        setStatus(runStatusEl, "run: idle", "neutral");
      }
      return report;
    }

    applyRunReport(report);
    return report;
  } catch (error) {
    const message = error instanceof Error ? error.message : "run command failed";
    setStatus(runStatusEl, `run: wasm error (${message})`, "error");
    return null;
  } finally {
    runBusy = false;
    applyRunControlState();
  }
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

async function sendDebugCommand(command: DebugCommandRequest): Promise<DebugReport | null> {
  if (!debugSessionActive && command.kind !== "stop") {
    return null;
  }

  debugBusy = true;
  applyDebugControlState();
  setStatus(debugStatusEl, `debug: ${command.kind}...`, "busy");

  try {
    const report = await debugCommandWithWasm(command);

    if (command.kind === "stop") {
      const message = report.commandOutput.trim();
      applyInactiveDebugState(message.length > 0 ? message : "debug session stopped");
      return report;
    }

    if (report.error === "debug session is not active") {
      applyInactiveDebugState("debug session is not active");
      return report;
    }

    debugSessionActive = !report.halted && !report.error;
    applyDebugReport(report);
    if (!debugSessionActive && !report.error) {
      applyDebugControlState();
    }
    return report;
  } catch (error) {
    const message = error instanceof Error ? error.message : "debug command failed";
    setStatus(debugStatusEl, `debug: wasm error (${message})`, "error");
    return null;
  } finally {
    debugBusy = false;
    applyDebugControlState();
  }
}

function updateHoverPanel(variable: string, value: string | null): void {
  if (!variable) {
    clearHoverInspect();
    return;
  }

  debugHoveredVar = variable;
  if (value === null) {
    debugHoverPanelEl.textContent = `hover inspect: ${variable} = (unavailable)`;
  } else {
    debugHoverPanelEl.textContent = `hover inspect: ${variable} = ${value}`;
  }
}

function hoverKey(variable: string): string {
  const line = debugCurrentLine ?? 0;
  return `${currentFlavor}:${line}:${variable}`;
}

async function resolveHoverValue(variable: string): Promise<string | null> {
  const key = hoverKey(variable);
  if (debugHoverCache.has(key)) {
    return debugHoverCache.get(key) ?? null;
  }

  const existing = debugHoverInflight.get(key);
  if (existing) {
    return existing;
  }

  const pending = (async () => {
    const report = await debugCommandWithWasm({ kind: "print_var", name: variable });
    if (report.error) {
      if (report.error === "debug session is not active") {
        applyInactiveDebugState("debug session is not active");
      }
      return null;
    }

    applyDebugReport(report, { showCommandOutput: false, updateStatus: false });
    const value = parseHoverValue(variable, report.commandOutput);
    debugHoverCache.set(key, value);
    return value;
  })();

  debugHoverInflight.set(key, pending);
  return pending.finally(() => {
    debugHoverInflight.delete(key);
  });
}

function installHoverProvider(languageId: string): void {
  monaco.languages.registerHoverProvider(languageId, {
    provideHover: async (hoverModel, position) => {
      if (!debugSessionActive) {
        return null;
      }

      const model = editor.getModel();
      if (!model || hoverModel.uri.toString() !== model.uri.toString()) {
        return null;
      }

      const word = hoverModel.getWordAtPosition(position);
      if (!word || !looksLikeIdentifier(word.word)) {
        return null;
      }

      const key = `${hoverKey(word.word)}:${position.lineNumber}:${word.startColumn}`;
      if (debugHoverActiveKey === key && debugHoveredVar === word.word) {
        const cached = debugHoverCache.get(hoverKey(word.word)) ?? null;
        if (cached === null) {
          return null;
        }
        return {
          range: new monaco.Range(position.lineNumber, word.startColumn, position.lineNumber, word.endColumn),
          contents: [{ value: `**${word.word}**` }, { value: `\`\`\`text\n${cached}\n\`\`\`` }]
        };
      }

      debugHoverActiveKey = key;
      debugHoverPanelEl.textContent = `hover inspect: ${word.word} = (loading)`;
      const value = await resolveHoverValue(word.word);
      if (debugHoverActiveKey !== key) {
        return null;
      }
      updateHoverPanel(word.word, value);
      if (value === null) {
        return null;
      }

      return {
        range: new monaco.Range(position.lineNumber, word.startColumn, position.lineNumber, word.endColumn),
        contents: [{ value: `**${word.word}**` }, { value: `\`\`\`text\n${value}\n\`\`\`` }]
      };
    }
  });
}

for (const flavor of FLAVOR_OPTIONS) {
  installHoverProvider(languageForFlavor(flavor.value));
}

editor.onMouseLeave(() => {
  clearHoverInspect();
});

editor.onMouseDown((event) => {
  if (debugBusy) {
    return;
  }

  if (
    event.target.type !== monaco.editor.MouseTargetType.GUTTER_GLYPH_MARGIN &&
    event.target.type !== monaco.editor.MouseTargetType.GUTTER_LINE_NUMBERS
  ) {
    return;
  }

  const line = event.target.position?.lineNumber;
  if (!line || line < 1) {
    return;
  }

  const breakpoints = activeBreakpoints();
  const exists = breakpoints.has(line);
  if (exists) {
    breakpoints.delete(line);
    applyDebugDecorations();
    if (debugSessionActive) {
      void sendDebugCommand({ kind: "clear_line", line });
    }
    return;
  }

  breakpoints.add(line);
  applyDebugDecorations();
  if (debugSessionActive) {
    void sendDebugCommand({ kind: "break_line", line });
  }
});

for (const [flavor, model] of Object.entries(models) as Array<[SourceFlavor, monaco.editor.ITextModel]>) {
  model.onDidChangeContent(() => {
    scheduleSourcePersist(flavor);
    if (editor.getModel() === model) {
      scheduleLint();
    }
  });
}

flavorSelectEl.value = currentFlavor;
persistCurrentFlavor(currentFlavor);
syncThemeButtons(themePreference);
flavorSelectEl.addEventListener("change", () => {
  const next = flavorSelectEl.value as SourceFlavor;
  if (!(next in models)) {
    return;
  }

  if (runSessionActive) {
    void runCommandWithWasm({ kind: "stop" }).catch(() => {
      // best effort stop
    });
  }
  if (debugSessionActive) {
    void debugCommandWithWasm({ kind: "stop" }).catch(() => {
      // best effort stop
    });
  }

  currentFlavor = next;
  persistCurrentFlavor(currentFlavor);
  editor.setModel(models[currentFlavor]);
  applyInactiveRunState();
  applyInactiveDebugState("<no debugger output>");
  applyDebugDecorations();
  setStatus(runStatusEl, "run: idle", "neutral");
  scheduleLint();
});

window.addEventListener("pagehide", () => {
  persistCurrentFlavor(currentFlavor);
  flushPersistedSources();
  persistFuelAmount(fuelAmountInputEl.value);
  persistFuelInterval(fuelIntervalInputEl.value);
});

for (const option of THEME_OPTIONS) {
  themeButtons[option.value].addEventListener("click", () => {
    applyThemePreference(option.value, true);
  });
}

fuelAmountInputEl.addEventListener("input", () => {
  persistFuelAmount(fuelAmountInputEl.value);
});

fuelIntervalInputEl.addEventListener("input", () => {
  persistFuelInterval(fuelIntervalInputEl.value);
});

const handleSystemThemeChange = () => {
  if (themePreference !== "system") {
    return;
  }
  applyThemePreference("system", false);
};

if (systemThemeQuery) {
  if (typeof systemThemeQuery.addEventListener === "function") {
    systemThemeQuery.addEventListener("change", handleSystemThemeChange);
  } else {
    systemThemeQuery.addListener(handleSystemThemeChange);
  }
}

runButtonEl.addEventListener("click", async () => {
  const fuelConfig = currentFuelConfig();
  if (!fuelConfig) {
    setStatus(runStatusEl, "run: invalid fuel settings", "error");
    return;
  }

  runBusy = true;
  applyRunControlState();
  setStatus(runStatusEl, "run: running...", "busy");

  const source = activeModel().getValue();
  try {
    const report = await runWithWasm(source, currentFlavor, fuelConfig);
    applyRunReport(report);
  } catch (error) {
    const message = error instanceof Error ? error.message : "run failed";
    setStatus(runStatusEl, `run: wasm error (${message})`, "error");
  } finally {
    runBusy = false;
    applyRunControlState();
  }
});

async function startDebugSession(): Promise<void> {
  if (debugBusy) {
    return;
  }

  const fuelConfig = currentFuelConfig();
  if (!fuelConfig) {
    setStatus(debugStatusEl, "debug: invalid fuel settings", "error");
    return;
  }

  debugBusy = true;
  applyDebugControlState();
  setStatus(debugStatusEl, "debug: starting...", "busy");

  const source = activeModel().getValue();
  const requestedBreakpoints = [...activeBreakpoints()].sort((lhs, rhs) => lhs - rhs);
  debugHoverCache.clear();
  debugHoverInflight.clear();
  clearHoverInspect();

  try {
    const startReport = await startDebugWithWasm(source, currentFlavor, fuelConfig);

    if (startReport.error) {
      debugSessionActive = false;
      applyDebugReport(startReport, { syncBreakpoints: false });
      return;
    }

    debugSessionActive = !startReport.halted;
    applyDebugReport(startReport, { syncBreakpoints: false });

    let latestReport = startReport;
    for (const line of requestedBreakpoints) {
      latestReport = await debugCommandWithWasm({ kind: "break_line", line });
      if (latestReport.error) {
        break;
      }
    }

    applyDebugReport(latestReport);
    debugSessionActive = !latestReport.halted && !latestReport.error;
    if (latestReport.error === "debug session is not active") {
      applyInactiveDebugState("debug session is not active");
      return;
    }

    if (debugSessionActive && requestedBreakpoints.length > 0) {
      setStatus(debugStatusEl, "debug: attached", "ok");
    }
  } catch (error) {
    const message = error instanceof Error ? error.message : "debug start failed";
    setStatus(debugStatusEl, `debug: wasm error (${message})`, "error");
    debugSessionActive = false;
    debugCurrentLine = null;
    applyDebugDecorations();
  } finally {
    debugBusy = false;
    applyDebugControlState();
  }
}

debugStartButtonEl.addEventListener("click", () => {
  void startDebugSession();
});

debugWhereButtonEl.addEventListener("click", () => {
  void sendDebugCommand({ kind: "where" });
});

debugLocalsButtonEl.addEventListener("click", () => {
  void sendDebugCommand({ kind: "locals" });
});

debugStackButtonEl.addEventListener("click", () => {
  void sendDebugCommand({ kind: "stack" });
});

debugStepButtonEl.addEventListener("click", () => {
  debugHoverCache.clear();
  debugHoverInflight.clear();
  clearHoverInspect();
  void sendDebugCommand({ kind: "step" });
});

debugNextButtonEl.addEventListener("click", () => {
  debugHoverCache.clear();
  debugHoverInflight.clear();
  clearHoverInspect();
  void sendDebugCommand({ kind: "next" });
});

debugOutButtonEl.addEventListener("click", () => {
  debugHoverCache.clear();
  debugHoverInflight.clear();
  clearHoverInspect();
  void sendDebugCommand({ kind: "out" });
});

debugContinueButtonEl.addEventListener("click", () => {
  debugHoverCache.clear();
  debugHoverInflight.clear();
  clearHoverInspect();
  void sendDebugCommand({ kind: "continue" });
});

debugStopButtonEl.addEventListener("click", () => {
  debugHoverCache.clear();
  debugHoverInflight.clear();
  clearHoverInspect();
  void sendDebugCommand({ kind: "stop" });
});

debugFuelSetButtonEl.addEventListener("click", () => {
  const parsed = readFuelForm();
  if (!parsed) {
    setStatus(debugStatusEl, "debug: invalid fuel settings", "error");
    return;
  }
  if (parsed.amount === null) {
    void sendDebugCommand({ kind: "clear_fuel" });
    return;
  }
  void sendDebugCommand({ kind: "set_fuel", amount: parsed.amount });
});

debugFuelAddButtonEl.addEventListener("click", () => {
  const parsed = readFuelForm();
  if (!parsed || parsed.amount === null) {
    setStatus(debugStatusEl, "debug: enter fuel to add", "error");
    return;
  }
  void sendDebugCommand({ kind: "add_fuel", amount: parsed.amount });
});

debugFuelIntervalButtonEl.addEventListener("click", () => {
  const parsed = readFuelForm();
  if (!parsed) {
    setStatus(debugStatusEl, "debug: invalid fuel settings", "error");
    return;
  }
  void sendDebugCommand({ kind: "set_fuel_check_interval", interval: parsed.interval });
});

runResumeButtonEl.addEventListener("click", () => {
  const parsed = readFuelForm();
  if (!parsed) {
    setStatus(runStatusEl, "run: invalid fuel settings", "error");
    return;
  }
  if (!runSessionActive) {
    return;
  }

  void (async () => {
    if (parsed.interval !== runFuelState.checkInterval) {
      const intervalReport = await sendRunCommand({
        kind: "set_fuel_check_interval",
        interval: parsed.interval
      });
      if (!intervalReport || intervalReport.error) {
        return;
      }
    }

    if (parsed.amount !== null) {
      const addReport = await sendRunCommand({ kind: "add_fuel", amount: parsed.amount });
      if (!addReport || addReport.error) {
        return;
      }
    }

    await sendRunCommand({ kind: "resume" });
  })();
});

applyInactiveRunState();
applyDebugControlState();
applyDebugDecorations();
scheduleLint();
