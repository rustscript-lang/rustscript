export type SourceFlavor = "rustscript" | "javascript" | "lua" | "scheme";

export type LintSpan = {
  startLine: number;
  startColumn: number;
  endLine: number;
  endColumn: number;
};

export type LintSeverity = "error" | "warning";

export type LintDiagnostic = {
  line: number;
  severity: LintSeverity;
  message: string;
  span: LintSpan | null;
  rendered: string;
};

export type LintReport = {
  diagnostics: LintDiagnostic[];
};

export type FormatReport = {
  ok: boolean;
  formatted: string | null;
  error: string | null;
};

export type RunReport = {
  ok: boolean;
  diagnostics: LintDiagnostic[];
  output: string[];
  stack: string[];
  error: string | null;
  halted: boolean;
  yielded: boolean;
  fuel: FuelState;
  commandOutput: string;
};

export type FuelConfig = {
  mode?: "fuel" | "epoch" | null;
  fuel: number | null;
  fuelCheckInterval: number | null;
  epochDeadline?: number | null;
  epochCheckInterval?: number | null;
};

export type FuelState = {
  enabled: boolean;
  mode: "none" | "fuel" | "epoch";
  remaining: number | null;
  checkInterval: number;
  epochCurrent: number;
  epochDeadline: number | null;
  epochSlice: number | null;
};

export type RunCommandRequest =
  | { kind: "resume" }
  | { kind: "set_fuel"; amount: number }
  | { kind: "add_fuel"; amount: number }
  | { kind: "clear_fuel" }
  | { kind: "set_fuel_check_interval"; interval: number }
  | { kind: "set_epoch_deadline"; ticks: number }
  | { kind: "clear_epoch_deadline" }
  | { kind: "tick_epoch"; amount: number }
  | { kind: "set_epoch_check_interval"; interval: number }
  | { kind: "stop" };

export type DebugCommandRequest =
  | { kind: "state" }
  | { kind: "continue" }
  | { kind: "step" }
  | { kind: "next" }
  | { kind: "out" }
  | { kind: "where" }
  | { kind: "locals" }
  | { kind: "stack" }
  | { kind: "print_var"; name: string }
  | { kind: "break_line"; line: number }
  | { kind: "clear_line"; line: number }
  | { kind: "set_fuel"; amount: number }
  | { kind: "add_fuel"; amount: number }
  | { kind: "clear_fuel" }
  | { kind: "set_fuel_check_interval"; interval: number }
  | { kind: "set_epoch_deadline"; ticks: number }
  | { kind: "clear_epoch_deadline" }
  | { kind: "tick_epoch"; amount: number }
  | { kind: "set_epoch_check_interval"; interval: number }
  | { kind: "stop" };

export type DebugReport = {
  diagnostics: LintDiagnostic[];
  output: string[];
  stack: string[];
  error: string | null;
  currentLine: number | null;
  breakpoints: number[];
  halted: boolean;
  commandOutput: string;
  fuel: FuelState;
};

export type CompletionEntryKind = "function" | "module" | "snippet";

export type CompletionEntry = {
  label: string;
  insertText: string;
  detail: string;
  documentation: string;
  kind: CompletionEntryKind;
};

export type CompletionCatalog = {
  rustscript: CompletionEntry[];
  javascript: CompletionEntry[];
  lua: CompletionEntry[];
  scheme: CompletionEntry[];
};

export type LocalTypeHint = {
  name: string;
  inferredType: string;
  declaredLine: number | null;
  lastLine: number | null;
};

type WasmRuntimeExports = {
  memory: WebAssembly.Memory;
  wasm_alloc(len: number): number;
  wasm_dealloc(ptr: number, len: number): void;
  lint_source_json(sourcePtr: number, sourceLen: number, flavorPtr: number, flavorLen: number): bigint;
  format_source_json?: (
    sourcePtr: number,
    sourceLen: number,
    flavorPtr: number,
    flavorLen: number
  ) => bigint;
  local_type_hints_json?: (
    sourcePtr: number,
    sourceLen: number,
    flavorPtr: number,
    flavorLen: number
  ) => bigint;
  run_source_json(
    sourcePtr: number,
    sourceLen: number,
    flavorPtr: number,
    flavorLen: number,
    optionsPtr: number,
    optionsLen: number
  ): bigint;
  run_command_json?: (commandPtr: number, commandLen: number) => bigint;
  debug_start_json?: (
    sourcePtr: number,
    sourceLen: number,
    flavorPtr: number,
    flavorLen: number,
    optionsPtr: number,
    optionsLen: number
  ) => bigint;
  debug_command_json?: (commandPtr: number, commandLen: number) => bigint;
  debug_state_json?: () => bigint;
  completion_catalog_json?: () => bigint;
};

const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8");
let wasmPromise: Promise<WasmRuntimeExports> | null = null;

function wasmPath(): string {
  const base = import.meta.env.BASE_URL ?? "/";
  return `${base.replace(/\/+$/, "/")}wasm/pd_vm_wasm.wasm`;
}

function writeBytes(wasm: WasmRuntimeExports, bytes: Uint8Array): number {
  if (bytes.length === 0) {
    return 0;
  }
  const ptr = wasm.wasm_alloc(bytes.length);
  new Uint8Array(wasm.memory.buffer).set(bytes, ptr);
  return ptr;
}

function unpackPtrLen(packed: bigint): { ptr: number; len: number } {
  const ptr = Number(packed & 0xFFFF_FFFFn);
  const len = Number((packed >> 32n) & 0xFFFF_FFFFn);
  return { ptr, len };
}

function decodeJsonPayload(wasm: WasmRuntimeExports, packed: bigint): unknown {
  const { ptr, len } = unpackPtrLen(packed);
  if (ptr === 0 || len === 0) {
    return null;
  }
  try {
    const bytes = new Uint8Array(wasm.memory.buffer, ptr, len);
    const json = decoder.decode(bytes);
    return JSON.parse(json);
  } finally {
    wasm.wasm_dealloc(ptr, len);
  }
}

function normalizeLintReport(raw: unknown): LintReport {
  if (!raw || typeof raw !== "object" || !("diagnostics" in raw)) {
    return { diagnostics: [] };
  }
  const rawDiagnostics = (raw as { diagnostics?: unknown }).diagnostics;
  if (!Array.isArray(rawDiagnostics)) {
    return { diagnostics: [] };
  }

  const diagnostics: LintDiagnostic[] = [];
  for (const entry of rawDiagnostics) {
    if (!entry || typeof entry !== "object") {
      continue;
    }
    const lineRaw = Number((entry as { line?: unknown }).line);
    const severityRaw = (entry as { severity?: unknown }).severity;
    const messageRaw = (entry as { message?: unknown }).message;
    const renderedRaw = (entry as { rendered?: unknown }).rendered;
    const spanRaw = (entry as { span?: unknown }).span;
    let span: LintSpan | null = null;

    if (spanRaw && typeof spanRaw === "object") {
      const startLine = Number((spanRaw as { start_line?: unknown }).start_line);
      const startCol = Number((spanRaw as { start_col?: unknown }).start_col);
      const endLine = Number((spanRaw as { end_line?: unknown }).end_line);
      const endCol = Number((spanRaw as { end_col?: unknown }).end_col);
      if (
        Number.isFinite(startLine) &&
        Number.isFinite(startCol) &&
        Number.isFinite(endLine) &&
        Number.isFinite(endCol)
      ) {
        span = {
          startLine: Math.max(1, Math.trunc(startLine)),
          startColumn: Math.max(1, Math.trunc(startCol)),
          endLine: Math.max(1, Math.trunc(endLine)),
          endColumn: Math.max(1, Math.trunc(endCol))
        };
      }
    }

    const line = Number.isFinite(lineRaw) ? Math.max(0, Math.trunc(lineRaw)) : 0;
    const severity: LintSeverity = severityRaw === "warning" ? "warning" : "error";
    const message = typeof messageRaw === "string" ? messageRaw : "";
    const rendered = typeof renderedRaw === "string" ? renderedRaw : message;
    if (!message) {
      continue;
    }
    diagnostics.push({ line, severity, message, rendered, span });
  }
  return { diagnostics };
}

function normalizeFormatReport(raw: unknown): FormatReport {
  if (!raw || typeof raw !== "object") {
    return {
      ok: false,
      formatted: null,
      error: "invalid format response"
    };
  }
  const rawOk = (raw as { ok?: unknown }).ok;
  const rawFormatted = (raw as { formatted?: unknown }).formatted;
  const rawError = (raw as { error?: unknown }).error;
  return {
    ok: typeof rawOk === "boolean" ? rawOk : typeof rawFormatted === "string",
    formatted: typeof rawFormatted === "string" ? rawFormatted : null,
    error: typeof rawError === "string" ? rawError : null
  };
}

function normalizeRunReport(raw: unknown): RunReport {
  if (!raw || typeof raw !== "object") {
    return {
      ok: false,
      diagnostics: [],
      output: [],
      stack: [],
      error: "invalid run response",
      halted: true,
      yielded: false,
      fuel: defaultFuelState(),
      commandOutput: ""
    };
  }
  const lint = normalizeLintReport(raw);
  const rawOutput = (raw as { output?: unknown }).output;
  const rawStack = (raw as { stack?: unknown }).stack;
  const rawError = (raw as { error?: unknown }).error;
  const rawOk = (raw as { ok?: unknown }).ok;
  const rawHalted = (raw as { halted?: unknown }).halted;
  const rawYielded = (raw as { yielded?: unknown }).yielded;
  const rawCommandOutput = (raw as { command_output?: unknown }).command_output;
  const rawFuel = (raw as { fuel?: unknown }).fuel;

  const output = Array.isArray(rawOutput)
    ? rawOutput.filter((entry): entry is string => typeof entry === "string")
    : [];
  const stack = Array.isArray(rawStack)
    ? rawStack.filter((entry): entry is string => typeof entry === "string")
    : [];
  const error = typeof rawError === "string" ? rawError : null;
  const ok = typeof rawOk === "boolean" ? rawOk : error === null && lint.diagnostics.length === 0;
  const halted = typeof rawHalted === "boolean" ? rawHalted : !ok;
  const yielded = typeof rawYielded === "boolean" ? rawYielded : false;
  const commandOutput = typeof rawCommandOutput === "string" ? rawCommandOutput : "";
  const fuel = normalizeFuelState(rawFuel);

  return {
    ok,
    diagnostics: lint.diagnostics,
    output,
    stack,
    error,
    halted,
    yielded,
    fuel,
    commandOutput
  };
}

function normalizeDebugReport(raw: unknown): DebugReport {
  if (!raw || typeof raw !== "object") {
    return {
      diagnostics: [],
      output: [],
      stack: [],
      error: "invalid debug response",
      currentLine: null,
      breakpoints: [],
      halted: true,
      commandOutput: "",
      fuel: defaultFuelState()
    };
  }

  const lint = normalizeLintReport(raw);
  const rawOutput = (raw as { output?: unknown }).output;
  const rawStack = (raw as { stack?: unknown }).stack;
  const rawError = (raw as { error?: unknown }).error;
  const rawCurrentLine = (raw as { current_line?: unknown }).current_line;
  const rawBreakpoints = (raw as { breakpoints?: unknown }).breakpoints;
  const rawHalted = (raw as { halted?: unknown }).halted;
  const rawCommandOutput = (raw as { command_output?: unknown }).command_output;
  const rawFuel = (raw as { fuel?: unknown }).fuel;

  const output = Array.isArray(rawOutput)
    ? rawOutput.filter((entry): entry is string => typeof entry === "string")
    : [];
  const stack = Array.isArray(rawStack)
    ? rawStack.filter((entry): entry is string => typeof entry === "string")
    : [];
  const error = typeof rawError === "string" ? rawError : null;
  const currentLine =
    typeof rawCurrentLine === "number" && Number.isFinite(rawCurrentLine)
      ? Math.max(1, Math.trunc(rawCurrentLine))
      : null;
  const breakpoints = Array.isArray(rawBreakpoints)
    ? rawBreakpoints
        .map((entry) => Number(entry))
        .filter((entry) => Number.isFinite(entry))
        .map((entry) => Math.max(1, Math.trunc(entry)))
    : [];
  const halted = typeof rawHalted === "boolean" ? rawHalted : false;
  const commandOutput = typeof rawCommandOutput === "string" ? rawCommandOutput : "";
  const fuel = normalizeFuelState(rawFuel);

  return {
    diagnostics: lint.diagnostics,
    output,
    stack,
    error,
    currentLine,
    breakpoints,
    halted,
    commandOutput,
    fuel
  };
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

function normalizeFuelState(raw: unknown): FuelState {
  if (!raw || typeof raw !== "object") {
    return defaultFuelState();
  }

  const rawEnabled = (raw as { enabled?: unknown }).enabled;
  const rawMode = (raw as { mode?: unknown }).mode;
  const rawRemaining = (raw as { remaining?: unknown }).remaining;
  const rawCheckInterval = (raw as { check_interval?: unknown }).check_interval;
  const rawEpochCurrent = (raw as { epoch_current?: unknown }).epoch_current;
  const rawEpochDeadline = (raw as { epoch_deadline?: unknown }).epoch_deadline;
  const rawEpochSlice = (raw as { epoch_slice?: unknown }).epoch_slice;

  return {
    enabled:
      typeof rawEnabled === "boolean"
        ? rawEnabled
        : rawRemaining !== null && rawRemaining !== undefined,
    mode: rawMode === "fuel" || rawMode === "epoch" || rawMode === "none" ? rawMode : "none",
    remaining:
      typeof rawRemaining === "number" && Number.isFinite(rawRemaining)
        ? Math.max(0, Math.trunc(rawRemaining))
        : null,
    checkInterval:
      typeof rawCheckInterval === "number" && Number.isFinite(rawCheckInterval)
        ? Math.max(1, Math.trunc(rawCheckInterval))
        : 1,
    epochCurrent:
      typeof rawEpochCurrent === "number" && Number.isFinite(rawEpochCurrent)
        ? Math.max(0, Math.trunc(rawEpochCurrent))
        : 0,
    epochDeadline:
      typeof rawEpochDeadline === "number" && Number.isFinite(rawEpochDeadline)
        ? Math.max(0, Math.trunc(rawEpochDeadline))
        : null,
    epochSlice:
      typeof rawEpochSlice === "number" && Number.isFinite(rawEpochSlice)
        ? Math.max(0, Math.trunc(rawEpochSlice))
        : null
  };
}

function normalizeCompletionKind(raw: unknown): CompletionEntryKind {
  if (raw === "function" || raw === "module" || raw === "snippet") {
    return raw;
  }
  return "snippet";
}

function normalizeCompletionEntry(raw: unknown): CompletionEntry | null {
  if (!raw || typeof raw !== "object") {
    return null;
  }
  const label = (raw as { label?: unknown }).label;
  const insertText = (raw as { insert_text?: unknown }).insert_text;
  const detail = (raw as { detail?: unknown }).detail;
  const documentation = (raw as { documentation?: unknown }).documentation;
  const kind = (raw as { kind?: unknown }).kind;

  if (typeof label !== "string" || typeof insertText !== "string") {
    return null;
  }

  return {
    label,
    insertText,
    detail: typeof detail === "string" ? detail : "",
    documentation: typeof documentation === "string" ? documentation : "",
    kind: normalizeCompletionKind(kind)
  };
}

function normalizeCompletionEntries(raw: unknown): CompletionEntry[] {
  if (!Array.isArray(raw)) {
    return [];
  }
  const entries: CompletionEntry[] = [];
  for (const candidate of raw) {
    const normalized = normalizeCompletionEntry(candidate);
    if (normalized) {
      entries.push(normalized);
    }
  }
  return entries;
}

function emptyCompletionCatalog(): CompletionCatalog {
  return {
    rustscript: [],
    javascript: [],
    lua: [],
    scheme: []
  };
}

function normalizeCompletionCatalog(raw: unknown): CompletionCatalog {
  if (!raw || typeof raw !== "object") {
    return emptyCompletionCatalog();
  }

  return {
    rustscript: normalizeCompletionEntries((raw as { rustscript?: unknown }).rustscript),
    javascript: normalizeCompletionEntries((raw as { javascript?: unknown }).javascript),
    lua: normalizeCompletionEntries((raw as { lua?: unknown }).lua),
    scheme: normalizeCompletionEntries((raw as { scheme?: unknown }).scheme)
  };
}

function normalizeLocalTypeHints(raw: unknown): LocalTypeHint[] {
  if (!raw || typeof raw !== "object" || !("hints" in raw)) {
    return [];
  }
  const rawHints = (raw as { hints?: unknown }).hints;
  if (!Array.isArray(rawHints)) {
    return [];
  }

  const hints: LocalTypeHint[] = [];
  for (const item of rawHints) {
    if (!item || typeof item !== "object") {
      continue;
    }
    const name = (item as { name?: unknown }).name;
    const inferredType = (item as { inferred_type?: unknown }).inferred_type;
    const declaredLineRaw = Number((item as { declared_line?: unknown }).declared_line);
    const lastLineRaw = Number((item as { last_line?: unknown }).last_line);
    if (typeof name !== "string" || !name || typeof inferredType !== "string" || !inferredType) {
      continue;
    }

    hints.push({
      name,
      inferredType,
      declaredLine: Number.isFinite(declaredLineRaw) ? Math.max(1, Math.trunc(declaredLineRaw)) : null,
      lastLine: Number.isFinite(lastLineRaw) ? Math.max(1, Math.trunc(lastLineRaw)) : null
    });
  }

  return hints;
}

async function loadWasm(): Promise<WasmRuntimeExports> {
  if (!wasmPromise) {
    wasmPromise = (async () => {
      const response = await fetch(wasmPath());
      if (!response.ok) {
        throw new Error(`failed to fetch playground wasm (${response.status})`);
      }
      const bytes = await response.arrayBuffer();
      const { instance } = await WebAssembly.instantiate(bytes, {
        env: {
          pd_playground_now_ms: () => globalThis.performance?.now?.() ?? Date.now()
        }
      });
      const exports = instance.exports as Partial<WasmRuntimeExports>;
      if (
        !exports.memory ||
        typeof exports.wasm_alloc !== "function" ||
        typeof exports.wasm_dealloc !== "function" ||
        typeof exports.lint_source_json !== "function" ||
        typeof exports.run_source_json !== "function"
      ) {
        throw new Error("invalid playground wasm exports");
      }
      return exports as WasmRuntimeExports;
    })();
  }
  return wasmPromise;
}

async function invokePackedJson(
  invoke: (
    wasm: WasmRuntimeExports,
    sourcePtr: number,
    sourceLen: number,
    flavorPtr: number,
    flavorLen: number,
    optionsPtr: number,
    optionsLen: number
  ) => bigint,
  source: string,
  flavor: SourceFlavor,
  options?: string
): Promise<unknown> {
  const wasm = await loadWasm();
  const sourceBytes = encoder.encode(source);
  const flavorBytes = encoder.encode(flavor);
  const optionsBytes = options ? encoder.encode(options) : new Uint8Array();
  let sourcePtr = 0;
  let flavorPtr = 0;
  let optionsPtr = 0;

  try {
    sourcePtr = writeBytes(wasm, sourceBytes);
    flavorPtr = writeBytes(wasm, flavorBytes);
    optionsPtr = writeBytes(wasm, optionsBytes);
    const packed = invoke(
      wasm,
      sourcePtr,
      sourceBytes.length,
      flavorPtr,
      flavorBytes.length,
      optionsPtr,
      optionsBytes.length
    );
    return decodeJsonPayload(wasm, packed);
  } finally {
    if (sourcePtr !== 0) {
      wasm.wasm_dealloc(sourcePtr, sourceBytes.length);
    }
    if (flavorPtr !== 0) {
      wasm.wasm_dealloc(flavorPtr, flavorBytes.length);
    }
    if (optionsPtr !== 0) {
      wasm.wasm_dealloc(optionsPtr, optionsBytes.length);
    }
  }
}

async function invokePackedJsonNoArgs(
  invoke: (wasm: WasmRuntimeExports) => bigint
): Promise<unknown> {
  const wasm = await loadWasm();
  const packed = invoke(wasm);
  return decodeJsonPayload(wasm, packed);
}

async function invokePackedJsonSingleArg(
  invoke: (wasm: WasmRuntimeExports, payloadPtr: number, payloadLen: number) => bigint,
  payload: string
): Promise<unknown> {
  const wasm = await loadWasm();
  const payloadBytes = encoder.encode(payload);
  let payloadPtr = 0;
  try {
    payloadPtr = writeBytes(wasm, payloadBytes);
    const packed = invoke(wasm, payloadPtr, payloadBytes.length);
    return decodeJsonPayload(wasm, packed);
  } finally {
    if (payloadPtr !== 0) {
      wasm.wasm_dealloc(payloadPtr, payloadBytes.length);
    }
  }
}

export async function lintWithWasm(source: string, flavor: SourceFlavor): Promise<LintReport> {
  const raw = await invokePackedJson(
    (wasm, sourcePtr, sourceLen, flavorPtr, flavorLen) =>
      wasm.lint_source_json(sourcePtr, sourceLen, flavorPtr, flavorLen),
    source,
    flavor
  );
  return normalizeLintReport(raw);
}

export async function formatWithWasm(source: string, flavor: SourceFlavor): Promise<FormatReport> {
  const wasm = await loadWasm();
  if (typeof wasm.format_source_json !== "function") {
    return {
      ok: false,
      formatted: null,
      error: "formatting wasm export is unavailable"
    };
  }
  const raw = await invokePackedJson(
    (instance, sourcePtr, sourceLen, flavorPtr, flavorLen) => {
      const invoke = instance.format_source_json;
      if (!invoke) {
        return 0n;
      }
      return invoke(sourcePtr, sourceLen, flavorPtr, flavorLen);
    },
    source,
    flavor
  );
  return normalizeFormatReport(raw);
}

export async function runWithWasm(
  source: string,
  flavor: SourceFlavor,
  fuelConfig: FuelConfig
): Promise<RunReport> {
  const raw = await invokePackedJson(
    (wasm, sourcePtr, sourceLen, flavorPtr, flavorLen, optionsPtr, optionsLen) =>
      wasm.run_source_json(sourcePtr, sourceLen, flavorPtr, flavorLen, optionsPtr, optionsLen),
    source,
    flavor,
    JSON.stringify(fuelConfig)
  );
  return normalizeRunReport(raw);
}

function unavailableRunReport(error: string): RunReport {
  return {
    diagnostics: [],
    output: [],
    stack: [],
    error,
    ok: false,
    halted: true,
    yielded: false,
    commandOutput: "",
    fuel: defaultFuelState()
  };
}

function unavailableDebugReport(error: string): DebugReport {
  return {
    diagnostics: [],
    output: [],
    stack: [],
    error,
    currentLine: null,
    breakpoints: [],
    halted: true,
    commandOutput: "",
    fuel: defaultFuelState()
  };
}

export async function runCommandWithWasm(command: RunCommandRequest): Promise<RunReport> {
  const wasm = await loadWasm();
  if (typeof wasm.run_command_json !== "function") {
    return unavailableRunReport("run session wasm export is unavailable");
  }
  const payload = JSON.stringify(command);
  const raw = await invokePackedJsonSingleArg((instance, payloadPtr, payloadLen) => {
    const invoke = instance.run_command_json;
    if (!invoke) {
      return 0n;
    }
    return invoke(payloadPtr, payloadLen);
  }, payload);
  return normalizeRunReport(raw);
}

export async function startDebugWithWasm(
  source: string,
  flavor: SourceFlavor,
  fuelConfig: FuelConfig
): Promise<DebugReport> {
  const wasm = await loadWasm();
  if (typeof wasm.debug_start_json !== "function") {
    return unavailableDebugReport("debugger wasm export is unavailable");
  }
  const raw = await invokePackedJson(
    (instance, sourcePtr, sourceLen, flavorPtr, flavorLen, optionsPtr, optionsLen) => {
      const invoke = instance.debug_start_json;
      if (!invoke) {
        return 0n;
      }
      return invoke(sourcePtr, sourceLen, flavorPtr, flavorLen, optionsPtr, optionsLen);
    },
    source,
    flavor,
    JSON.stringify(fuelConfig)
  );
  return normalizeDebugReport(raw);
}

export async function debugCommandWithWasm(command: DebugCommandRequest): Promise<DebugReport> {
  const wasm = await loadWasm();
  if (typeof wasm.debug_command_json !== "function") {
    return unavailableDebugReport("debugger wasm export is unavailable");
  }
  const payload = JSON.stringify(command);
  const raw = await invokePackedJsonSingleArg((instance, payloadPtr, payloadLen) => {
    const invoke = instance.debug_command_json;
    if (!invoke) {
      return 0n;
    }
    return invoke(payloadPtr, payloadLen);
  }, payload);
  return normalizeDebugReport(raw);
}

export async function debugStateWithWasm(): Promise<DebugReport> {
  const wasm = await loadWasm();
  if (typeof wasm.debug_state_json !== "function") {
    return unavailableDebugReport("debugger wasm export is unavailable");
  }
  const raw = await invokePackedJsonNoArgs((instance) => {
    const invoke = instance.debug_state_json;
    if (!invoke) {
      return 0n;
    }
    return invoke();
  });
  return normalizeDebugReport(raw);
}

export async function completionCatalogWithWasm(): Promise<CompletionCatalog> {
  const wasm = await loadWasm();
  if (typeof wasm.completion_catalog_json !== "function") {
    return emptyCompletionCatalog();
  }
  const raw = await invokePackedJsonNoArgs((instance) => {
    const invoke = instance.completion_catalog_json;
    if (!invoke) {
      return 0n;
    }
    return invoke();
  });
  return normalizeCompletionCatalog(raw);
}

export async function localTypeHintsWithWasm(
  source: string,
  flavor: SourceFlavor
): Promise<LocalTypeHint[]> {
  const wasm = await loadWasm();
  if (typeof wasm.local_type_hints_json !== "function") {
    return [];
  }
  const raw = await invokePackedJson(
    (instance, sourcePtr, sourceLen, flavorPtr, flavorLen) => {
      const invoke = instance.local_type_hints_json;
      if (!invoke) {
        return 0n;
      }
      return invoke(sourcePtr, sourceLen, flavorPtr, flavorLen);
    },
    source,
    flavor
  );
  return normalizeLocalTypeHints(raw);
}
