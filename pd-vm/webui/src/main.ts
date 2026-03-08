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
  BREAKPOINT_GLYPH_CLASS,
  CURRENT_LINE_CLASS,
  CURRENT_LINE_MARKER_CLASS,
  DEFAULT_FUEL_HINT,
  EPOCH_FLUSH_INTERVAL_MS,
  EPOCH_TICK_INTERVAL_MS,
  EPOCH_UI_REFRESH_INTERVAL_MS,
  FLAVOR_OPTIONS,
  FLAVOR_STORAGE_KEY,
  MARKER_OWNER,
  RUN_POLL_INTERVAL_MS,
  SAMPLE_SOURCES,
  THEME_OPTIONS,
  THEME_STORAGE_KEY,
  applyDocumentTheme,
  languageForFlavor,
  loadCurrentFlavor,
  loadSourceForFlavor,
  loadThemePreference,
  resolveTheme,
  sourceStorageKey,
  updateViewportHeightCssVar,
  type InterruptModeChoice,
  type ResolvedTheme,
  type ThemePreference
} from "./playgroundConfig";
import { mountPlaygroundUi } from "./playgroundShell";
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

const systemThemeQuery =
  typeof window !== "undefined" && typeof window.matchMedia === "function"
    ? window.matchMedia("(prefers-color-scheme: dark)")
    : null;
const initialFlavor = loadCurrentFlavor();
const initialInterruptMode: InterruptModeChoice = "none";
const initialFuelAmount = "10";
const initialFuelInterval = "1";
const initialThemePreference = loadThemePreference();
const initialResolvedTheme = resolveTheme(initialThemePreference, systemThemeQuery);
updateViewportHeightCssVar();
applyDocumentTheme(initialResolvedTheme);

const app = document.querySelector<HTMLDivElement>("#app");
if (!app) {
  throw new Error("app root not found");
}

const {
  flavorSelectEl,
  themeControlEl,
  themeSystemButtonEl,
  themeLightButtonEl,
  themeDarkButtonEl,
  runButtonEl,
  debugStartButtonEl,
  debugWhereButtonEl,
  debugLocalsButtonEl,
  debugStackButtonEl,
  debugStepButtonEl,
  debugNextButtonEl,
  debugOutButtonEl,
  debugContinueButtonEl,
  stopButtonEl,
  lintStatusEl,
  sessionStatusEl,
  loadSampleButtonEl,
  diagnosticsPanelEl,
  outputPanelEl,
  stackPanelEl,
  debugOutputPanelEl,
  debugHoverPanelEl,
  interruptModeSelectEl,
  fuelAmountLabelEl,
  fuelIntervalLabelEl,
  fuelAmountInputEl,
  fuelIntervalInputEl,
  debugFuelSetButtonEl,
  debugFuelAddButtonEl,
  debugFuelIntervalButtonEl,
  debugEpochTickButtonEl,
  runResumeButtonEl,
  fuelHintPanelEl,
  runFuelStatePanelEl,
  runEpochStatePanelEl,
  debugFuelStatePanelEl,
  debugEpochStatePanelEl,
  editorHostEl,
  panelController
} = mountPlaygroundUi(app, DEFAULT_FUEL_HINT);

interruptModeSelectEl.value = initialInterruptMode;
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

function refreshViewportLayout(): void {
  updateViewportHeightCssVar();
  refreshEditorGeometry();
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

window.addEventListener("resize", refreshViewportLayout);
window.visualViewport?.addEventListener("resize", refreshViewportLayout);
window.visualViewport?.addEventListener("scroll", refreshViewportLayout);

let currentFlavor: SourceFlavor = initialFlavor;
let interruptMode: InterruptModeChoice = initialInterruptMode;
let themePreference: ThemePreference = initialThemePreference;
let resolvedTheme: ResolvedTheme = initialResolvedTheme;
let lintSequence = 0;
let lintTimer: number | null = null;
const sourcePersistTimers = new Map<SourceFlavor, number>();
let runBusy = false;
let runSessionActive = false;
let runSessionYielded = false;
let runFuelState: FuelState = defaultFuelState();
let runPollTimer: number | null = null;
let runPollGeneration = 0;
let pendingRunEpochTicks = 0;
let debugBusy = false;
let debugSessionActive = false;
let debugCurrentLine: number | null = null;
let debugFuelState: FuelState = defaultFuelState();
let pendingDebugEpochTicks = 0;
let debugHoveredVar = "";
let debugHoverActiveKey = "";
let debugDecorationIds: string[] = [];
const debugHoverCache = new Map<string, string | null>();
const debugHoverInflight = new Map<string, Promise<string | null>>();
let epochTickTimer: number | null = null;
let epochTickerPaused = false;
let epochFlushInFlight = false;
let lastEpochUiRefreshAt = 0;
let lastEpochFlushAt = 0;
type StatusClass = "neutral" | "ok" | "error" | "busy";
type SessionStatusSnapshot = {
  text: string;
  className: StatusClass;
};
let runStatusState: SessionStatusSnapshot = { text: "idle", className: "neutral" };
let debugStatusState: SessionStatusSnapshot = { text: "idle", className: "neutral" };
const themeButtons: Record<ThemePreference, HTMLButtonElement> = {
  system: themeSystemButtonEl,
  light: themeLightButtonEl,
  dark: themeDarkButtonEl
};

function setStatus(node: HTMLElement, text: string, className: StatusClass): void {
  node.classList.remove("neutral", "ok", "error", "busy");
  node.classList.add(className);
  node.textContent = text;
}

function statusPriority(source: "run" | "debug", status: SessionStatusSnapshot): number {
  if (status.className === "busy") {
    return 50;
  }
  if (status.className === "ok" && status.text !== "completed") {
    return 45;
  }
  if (status.className === "error") {
    return 40;
  }
  if (status.className === "ok") {
    return 10;
  }
  if (status.text !== "idle") {
    return 5;
  }
  return source === "debug" ? 1 : 0;
}

function syncSessionStatus(): void {
  const nextStatus =
    statusPriority("debug", debugStatusState) >= statusPriority("run", runStatusState) ? debugStatusState : runStatusState;
  setStatus(sessionStatusEl, nextStatus.text, nextStatus.className);
}

function setRunSessionStatus(text: string, className: StatusClass): void {
  runStatusState = { text, className };
  syncSessionStatus();
}

function setDebugSessionStatus(text: string, className: StatusClass): void {
  debugStatusState = { text, className };
  syncSessionStatus();
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
    mode: "none",
    remaining: null,
    checkInterval: 1,
    epochCurrent: 0,
    epochDeadline: null,
    epochSlice: null
  };
}

function formatFuelState(fuel: FuelState, pendingEpochTicks = 0): string {
  if (fuel.mode === "epoch") {
    const deadline = fuel.epochDeadline === null ? "disabled" : String(fuel.epochDeadline);
    const slice = fuel.epochSlice === null ? "disabled" : String(fuel.epochSlice);
    return `epoch current=${fuel.epochCurrent + pendingEpochTicks} deadline=${deadline} slice=${slice} (interval ${fuel.checkInterval})`;
  }
  if (fuel.mode === "fuel") {
    return `${fuel.remaining ?? 0} left (interval ${fuel.checkInterval})`;
  }
  return `disabled (interval ${fuel.checkInterval})`;
}

function setFuelHint(message: string | null): void {
  fuelHintPanelEl.textContent = message ?? DEFAULT_FUEL_HINT;
}

function syncInterruptFormUi(): void {
  const epochMode = interruptMode === "epoch";
  const interruptDisabled = interruptMode === "none";
  fuelAmountLabelEl.textContent = epochMode ? "Epoch Deadline (ms)" : "Fuel Amount";
  fuelIntervalLabelEl.textContent = "Check Interval";
  fuelAmountInputEl.placeholder = epochMode ? "disabled" : "disabled";
  fuelAmountInputEl.disabled = interruptDisabled;
  fuelIntervalInputEl.disabled = interruptDisabled;
  debugFuelSetButtonEl.textContent = epochMode ? "Arm Debug Epoch" : "Set Debug Fuel";
  debugFuelAddButtonEl.textContent = epochMode ? "Clear Debug Epoch" : "Add Debug Fuel";
  debugFuelIntervalButtonEl.textContent = epochMode ? "Apply Epoch Interval" : "Apply Debug Interval";
  debugEpochTickButtonEl.textContent = epochTickerPaused ? "Resume Tick" : "Pause Tick";
  debugEpochTickButtonEl.hidden = !epochMode;
  debugEpochTickButtonEl.disabled = !epochMode || debugBusy || (!debugSessionActive && !runSessionActive);
}

function epochResumeHint(): string {
  return "Run paused at an epoch deadline. Resume automatically re-arms the same epoch slice. Browser epoch ticks come from a 1ms JS timer and only advance while the main thread can process them.";
}

function runEpochTimerActive(): boolean {
  return runSessionActive && runFuelState.mode === "epoch";
}

function debugEpochTimerActive(): boolean {
  return debugSessionActive && debugFuelState.mode === "epoch";
}

function epochTimerShouldRun(): boolean {
  return runEpochTimerActive() || debugEpochTimerActive();
}

function syncEpochPendingState(): void {
  if (!runEpochTimerActive()) {
    pendingRunEpochTicks = 0;
  }
  if (!debugEpochTimerActive()) {
    pendingDebugEpochTicks = 0;
  }
}

async function flushPendingRunEpochTicks(): Promise<void> {
  if (!runEpochTimerActive() || runBusy || pendingRunEpochTicks <= 0) {
    return;
  }

  const amount = pendingRunEpochTicks;
  const report = await runCommandWithWasm({ kind: "tick_epoch", amount });
  if (report.error === "run session is not active" || report.halted) {
    applyInactiveRunState();
    return;
  }
  if (report.error) {
    return;
  }

  pendingRunEpochTicks = Math.max(0, pendingRunEpochTicks - amount);
  runFuelState = report.fuel;
  syncFuelPanel();
}

async function flushPendingDebugEpochTicks(): Promise<void> {
  if (!debugEpochTimerActive() || debugBusy || pendingDebugEpochTicks <= 0) {
    return;
  }

  const amount = pendingDebugEpochTicks;
  const report = await debugCommandWithWasm({ kind: "tick_epoch", amount });
  if (report.error === "debug session is not active" || report.halted) {
    applyInactiveDebugState("debug session is not active");
    return;
  }
  if (report.error) {
    return;
  }

  pendingDebugEpochTicks = Math.max(0, pendingDebugEpochTicks - amount);
  debugFuelState = report.fuel;
  syncFuelPanel();
}

async function flushPendingEpochTicks(): Promise<void> {
  if (epochFlushInFlight) {
    return;
  }
  if (!epochTimerShouldRun()) {
    return;
  }

  epochFlushInFlight = true;
  try {
    await flushPendingRunEpochTicks();
    await flushPendingDebugEpochTicks();
  } finally {
    epochFlushInFlight = false;
  }
}

function stopEpochTicker(): void {
  if (epochTickTimer !== null) {
    window.clearTimeout(epochTickTimer);
    epochTickTimer = null;
  }
}

function scheduleEpochTicker(): void {
  if (epochTickTimer !== null || epochTickerPaused || !epochTimerShouldRun()) {
    return;
  }

  epochTickTimer = window.setTimeout(() => {
    epochTickTimer = null;
    if (!epochTimerShouldRun()) {
      syncEpochPendingState();
      syncFuelPanel();
      return;
    }

    if (runEpochTimerActive()) {
      pendingRunEpochTicks += 1;
    }
    if (debugEpochTimerActive()) {
      pendingDebugEpochTicks += 1;
    }

    const now = Date.now();
    if (now - lastEpochUiRefreshAt >= EPOCH_UI_REFRESH_INTERVAL_MS) {
      lastEpochUiRefreshAt = now;
      syncFuelPanel();
    }
    if (now - lastEpochFlushAt >= EPOCH_FLUSH_INTERVAL_MS) {
      lastEpochFlushAt = now;
      void flushPendingEpochTicks();
    }
    scheduleEpochTicker();
  }, EPOCH_TICK_INTERVAL_MS);
}

function syncEpochTicker(): void {
  syncEpochPendingState();
  if (!epochTimerShouldRun()) {
    stopEpochTicker();
    epochTickerPaused = false;
    return;
  }
  if (epochTickerPaused) {
    stopEpochTicker();
    return;
  }
  scheduleEpochTicker();
}

function cancelRunPolling(): void {
  runPollGeneration += 1;
  if (runPollTimer !== null) {
    window.clearTimeout(runPollTimer);
    runPollTimer = null;
  }
}

function scheduleRunPolling(): void {
  if (!runSessionActive || runSessionYielded || runBusy || runPollTimer !== null) {
    return;
  }

  const token = runPollGeneration;
  runPollTimer = window.setTimeout(() => {
    runPollTimer = null;
    if (token !== runPollGeneration || !runSessionActive || runSessionYielded || runBusy) {
      return;
    }
    void sendRunCommand({ kind: "resume" });
  }, RUN_POLL_INTERVAL_MS);
}

function scheduleRunPollingIfActive(): void {
  if (runSessionActive && !runSessionYielded) {
    scheduleRunPolling();
  }
}

function setInterruptStateLine(node: HTMLElement, visible: boolean, text: string): void {
  node.hidden = !visible;
  if (visible) {
    node.textContent = text;
  }
}

function syncFuelPanel(): void {
  syncEpochPendingState();
  const interruptDisabled = interruptMode === "none";
  setInterruptStateLine(
    runFuelStatePanelEl,
    runSessionActive && runFuelState.mode === "fuel",
    `Run fuel: ${formatFuelState(runFuelState, pendingRunEpochTicks)}`
  );
  setInterruptStateLine(
    runEpochStatePanelEl,
    runSessionActive && runFuelState.mode === "epoch",
    `Run epoch: ${formatFuelState(runFuelState, pendingRunEpochTicks)}`
  );
  setInterruptStateLine(
    debugFuelStatePanelEl,
    debugSessionActive && debugFuelState.mode === "fuel",
    `Debug fuel: ${formatFuelState(debugFuelState, pendingDebugEpochTicks)}`
  );
  setInterruptStateLine(
    debugEpochStatePanelEl,
    debugSessionActive && debugFuelState.mode === "epoch",
    `Debug epoch: ${formatFuelState(debugFuelState, pendingDebugEpochTicks)}`
  );
  runResumeButtonEl.disabled = !runSessionActive || runBusy;
  debugFuelSetButtonEl.disabled = interruptDisabled || !debugSessionActive || debugBusy;
  debugFuelAddButtonEl.disabled = interruptDisabled || !debugSessionActive || debugBusy;
  debugFuelIntervalButtonEl.disabled = interruptDisabled || !debugSessionActive || debugBusy;
  debugEpochTickButtonEl.disabled = interruptMode !== "epoch" || debugBusy || (!debugSessionActive && !runSessionActive);
  syncEpochTicker();
  syncInterruptFormUi();
}

type ParsedFuelForm = {
  mode: InterruptModeChoice;
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
      setFuelHint(
        interruptMode === "epoch"
          ? "Epoch deadline must be a non-negative integer."
          : "Fuel amount must be a non-negative integer."
      );
      return null;
    }
    amount = parsedAmount;
  }

  const parsedInterval = Number(rawInterval);
  if (!Number.isSafeInteger(parsedInterval) || parsedInterval < 1) {
    setFuelHint("Fuel check interval must be an integer greater than or equal to 1.");
    return null;
  }

  setFuelHint(
    runSessionYielded
      ? runFuelState.mode === "epoch"
        ? epochResumeHint()
        : "Run paused. Enter more fuel and click Resume Run."
      : null
  );
  return {
    mode: interruptMode,
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
    mode: parsed.mode === "none" ? null : parsed.mode,
    fuel: parsed.mode === "fuel" ? parsed.amount : null,
    fuelCheckInterval: parsed.mode === "fuel" ? parsed.interval : null,
    epochDeadline: parsed.mode === "epoch" ? parsed.amount : null,
    epochCheckInterval: parsed.mode === "epoch" ? parsed.interval : null
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

function resetEditorToSampleSource(): void {
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

  activeModel().setValue(SAMPLE_SOURCES[currentFlavor]);
  monaco.editor.setModelMarkers(activeModel(), MARKER_OWNER, []);
  renderDiagnosticsList(diagnosticsPanelEl, []);
  setRunPanel(outputPanelEl, stackPanelEl, [], []);
  applyInactiveRunState();
  applyInactiveDebugState("<no debugger output>");
  setStatus(lintStatusEl, "lint: queued...", "busy");
  scheduleLint();
  editor.focus();
}

function clearHoverInspect(): void {
  debugHoveredVar = "";
  debugHoverActiveKey = "";
  debugHoverPanelEl.textContent = "hover inspect: (none)";
}

function syncStopButtonState(): void {
  const canStopDebug = debugSessionActive && !debugBusy;
  const canStopRun = runSessionActive && !runBusy;
  stopButtonEl.disabled = !(canStopDebug || canStopRun);
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
  syncStopButtonState();
  syncFuelPanel();
}

function applyRunControlState(): void {
  runButtonEl.disabled = runBusy;
  syncStopButtonState();
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
    setDebugSessionStatus(`error (${report.error})`, "error");
    return;
  }
  if (report.currentLine) {
    setDebugSessionStatus(`paused @ ${report.currentLine}`, "ok");
    return;
  }
  if (report.halted) {
    setDebugSessionStatus("completed", "ok");
    return;
  }
  setDebugSessionStatus("running", "busy");
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
  setDebugSessionStatus("idle", "neutral");
}

function applyInactiveRunState(): void {
  cancelRunPolling();
  runBusy = false;
  runSessionActive = false;
  runSessionYielded = false;
  runFuelState = defaultFuelState();
  setFuelHint(null);
  applyRunControlState();
  setRunSessionStatus("idle", "neutral");
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

  if (report.error) {
    cancelRunPolling();
    setRunSessionStatus(`error (${report.error})`, "error");
  } else if (report.halted) {
    cancelRunPolling();
    setRunSessionStatus("completed", "ok");
  } else {
    setRunSessionStatus("running", "busy");
  }

  setFuelHint(
    runSessionYielded
      ? runFuelState.mode === "epoch"
        ? epochResumeHint()
        : "Run paused. Enter more fuel and click Resume Run."
      : null
  );
  syncFuelPanel();
  if (runSessionActive && !runSessionYielded && !report.error) {
    scheduleRunPolling();
  }
}

async function sendRunCommand(command: RunCommandRequest): Promise<RunReport | null> {
  if (!runSessionActive && command.kind !== "stop") {
    return null;
  }

  if (command.kind !== "stop" && command.kind !== "tick_epoch" && runFuelState.mode === "epoch") {
    await flushPendingRunEpochTicks();
  }
  if (!runSessionActive && command.kind !== "stop") {
    return null;
  }

  cancelRunPolling();
  runBusy = true;
  applyRunControlState();
  if (command.kind !== "stop") {
    setRunSessionStatus("running", "busy");
  }

  try {
    const report = await runCommandWithWasm(command);
    if (command.kind === "stop" || report.error === "run session is not active") {
      applyInactiveRunState();
      if (report.error) {
        setRunSessionStatus(`error (${report.error})`, "error");
      }
      return report;
    }

    applyRunReport(report);
    return report;
  } catch (error) {
    const message = error instanceof Error ? error.message : "run command failed";
    setRunSessionStatus(`error (${message})`, "error");
    return null;
  } finally {
    runBusy = false;
    applyRunControlState();
    scheduleRunPollingIfActive();
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

  if (command.kind !== "stop" && command.kind !== "tick_epoch" && debugFuelState.mode === "epoch") {
    await flushPendingDebugEpochTicks();
  }
  if (!debugSessionActive && command.kind !== "stop") {
    return null;
  }

  debugBusy = true;
  applyDebugControlState();
  setDebugSessionStatus("running", "busy");

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
    setDebugSessionStatus(`error (${message})`, "error");
    return null;
  } finally {
    debugBusy = false;
    applyDebugControlState();
  }
}

async function stopActiveSessions(): Promise<void> {
  if (debugSessionActive && !debugBusy) {
    await sendDebugCommand({ kind: "stop" });
  }
  if (runSessionActive && !runBusy) {
    await sendRunCommand({ kind: "stop" });
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
  scheduleLint();
});

loadSampleButtonEl.addEventListener("click", () => {
  resetEditorToSampleSource();
});

window.addEventListener("pagehide", () => {
  persistCurrentFlavor(currentFlavor);
  flushPersistedSources();
});

for (const option of THEME_OPTIONS) {
  themeButtons[option.value].addEventListener("click", () => {
    applyThemePreference(option.value, true);
  });
}

interruptModeSelectEl.addEventListener("change", () => {
  interruptMode =
    interruptModeSelectEl.value === "epoch"
      ? "epoch"
      : interruptModeSelectEl.value === "fuel"
        ? "fuel"
        : "none";
  syncFuelPanel();
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
    setRunSessionStatus("invalid interruption settings", "error");
    return;
  }

  cancelRunPolling();
  runBusy = true;
  applyRunControlState();
  setRunSessionStatus("running", "busy");

  const source = activeModel().getValue();
  try {
    const report = await runWithWasm(source, currentFlavor, fuelConfig);
    applyRunReport(report);
  } catch (error) {
    const message = error instanceof Error ? error.message : "run failed";
    setRunSessionStatus(`error (${message})`, "error");
  } finally {
    runBusy = false;
    applyRunControlState();
    scheduleRunPollingIfActive();
  }
});

async function startDebugSession(): Promise<void> {
  if (debugBusy) {
    return;
  }

  const fuelConfig = currentFuelConfig();
  if (!fuelConfig) {
    setDebugSessionStatus("invalid interruption settings", "error");
    return;
  }

  debugBusy = true;
  applyDebugControlState();
  setDebugSessionStatus("running", "busy");

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
    if (debugSessionActive) {
      panelController.expand(["debug", "fuel"]);
    }
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

    if (debugSessionActive) {
      panelController.expand(["debug", "fuel"]);
    }

    if (debugSessionActive && requestedBreakpoints.length > 0) {
      setDebugSessionStatus("running", "busy");
    }
  } catch (error) {
    const message = error instanceof Error ? error.message : "debug start failed";
    setDebugSessionStatus(`error (${message})`, "error");
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

stopButtonEl.addEventListener("click", () => {
  debugHoverCache.clear();
  debugHoverInflight.clear();
  clearHoverInspect();
  void stopActiveSessions();
});

debugFuelSetButtonEl.addEventListener("click", () => {
  const parsed = readFuelForm();
  if (!parsed) {
    setDebugSessionStatus("invalid interruption settings", "error");
    return;
  }
  if (parsed.mode === "none") {
    setDebugSessionStatus("select fuel or epoch mode", "error");
    return;
  }
  if (parsed.mode === "epoch") {
    if (parsed.amount === null) {
      void sendDebugCommand({ kind: "clear_epoch_deadline" });
      return;
    }
    void sendDebugCommand({ kind: "set_epoch_deadline", ticks: parsed.amount });
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
  if (!parsed) {
    setDebugSessionStatus("invalid interruption settings", "error");
    return;
  }
  if (parsed.mode === "none") {
    setDebugSessionStatus("select fuel or epoch mode", "error");
    return;
  }
  if (parsed.mode === "epoch") {
    void sendDebugCommand({ kind: "clear_epoch_deadline" });
    return;
  }
  if (parsed.amount === null) {
    setDebugSessionStatus("enter fuel to add", "error");
    return;
  }
  void sendDebugCommand({ kind: "add_fuel", amount: parsed.amount });
});

debugFuelIntervalButtonEl.addEventListener("click", () => {
  const parsed = readFuelForm();
  if (!parsed) {
    setDebugSessionStatus("invalid interruption settings", "error");
    return;
  }
  if (parsed.mode === "none") {
    setDebugSessionStatus("select fuel or epoch mode", "error");
    return;
  }
  void sendDebugCommand(
    parsed.mode === "epoch"
      ? { kind: "set_epoch_check_interval", interval: parsed.interval }
      : { kind: "set_fuel_check_interval", interval: parsed.interval }
  );
});

debugEpochTickButtonEl.addEventListener("click", () => {
  if (interruptMode !== "epoch") {
    return;
  }
  if (!debugSessionActive && !runSessionActive) {
    return;
  }
  epochTickerPaused = !epochTickerPaused;
  syncFuelPanel();
});

runResumeButtonEl.addEventListener("click", () => {
  const parsed = readFuelForm();
  if (!parsed) {
    setRunSessionStatus("invalid interruption settings", "error");
    return;
  }
  if (parsed.mode === "none") {
    setRunSessionStatus("select fuel or epoch mode", "error");
    return;
  }
  if (!runSessionActive) {
    return;
  }

  void (async () => {
    if (parsed.mode === "epoch") {
      if (parsed.interval !== runFuelState.checkInterval || runFuelState.mode !== "epoch") {
        const intervalReport = await sendRunCommand({
          kind: "set_epoch_check_interval",
          interval: parsed.interval
        });
        if (!intervalReport || intervalReport.error) {
          return;
        }
      }

      if (
        parsed.amount !== null &&
        (runFuelState.mode !== "epoch" ||
          runFuelState.epochDeadline === null ||
          runFuelState.epochSlice !== parsed.amount)
      ) {
        const deadlineReport = await sendRunCommand({
          kind: "set_epoch_deadline",
          ticks: parsed.amount
        });
        if (!deadlineReport || deadlineReport.error) {
          return;
        }
      } else if (runFuelState.mode === "epoch" && runFuelState.epochDeadline !== null) {
        const clearReport = await sendRunCommand({ kind: "clear_epoch_deadline" });
        if (!clearReport || clearReport.error) {
          return;
        }
      }

      await sendRunCommand({ kind: "resume" });
      return;
    }

    if (parsed.interval !== runFuelState.checkInterval || runFuelState.mode !== "fuel") {
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
