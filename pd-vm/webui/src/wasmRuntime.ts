export type SourceFlavor = "rustscript" | "javascript" | "lua" | "scheme";

export type LintSpan = {
  startLine: number;
  startColumn: number;
  endLine: number;
  endColumn: number;
};

export type LintDiagnostic = {
  line: number;
  message: string;
  span: LintSpan | null;
  rendered: string;
};

export type LintReport = {
  diagnostics: LintDiagnostic[];
};

export type RunReport = {
  ok: boolean;
  diagnostics: LintDiagnostic[];
  output: string[];
  stack: string[];
  error: string | null;
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

type WasmRuntimeExports = {
  memory: WebAssembly.Memory;
  wasm_alloc(len: number): number;
  wasm_dealloc(ptr: number, len: number): void;
  lint_source_json(sourcePtr: number, sourceLen: number, flavorPtr: number, flavorLen: number): bigint;
  run_source_json(sourcePtr: number, sourceLen: number, flavorPtr: number, flavorLen: number): bigint;
  completion_catalog_json?: () => bigint;
};

const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8");
let wasmPromise: Promise<WasmRuntimeExports> | null = null;

function wasmPath(): string {
  const base = import.meta.env.BASE_URL ?? "/";
  return `${base.replace(/\/+$/, "/")}wasm/pd_vm_runtime_wasm.wasm`;
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
    const message = typeof messageRaw === "string" ? messageRaw : "";
    const rendered = typeof renderedRaw === "string" ? renderedRaw : message;
    if (!message) {
      continue;
    }
    diagnostics.push({ line, message, rendered, span });
  }
  return { diagnostics };
}

function normalizeRunReport(raw: unknown): RunReport {
  if (!raw || typeof raw !== "object") {
    return { ok: false, diagnostics: [], output: [], stack: [], error: "invalid run response" };
  }
  const lint = normalizeLintReport(raw);
  const rawOutput = (raw as { output?: unknown }).output;
  const rawStack = (raw as { stack?: unknown }).stack;
  const rawError = (raw as { error?: unknown }).error;
  const rawOk = (raw as { ok?: unknown }).ok;

  const output = Array.isArray(rawOutput)
    ? rawOutput.filter((entry): entry is string => typeof entry === "string")
    : [];
  const stack = Array.isArray(rawStack)
    ? rawStack.filter((entry): entry is string => typeof entry === "string")
    : [];
  const error = typeof rawError === "string" ? rawError : null;
  const ok = typeof rawOk === "boolean" ? rawOk : error === null && lint.diagnostics.length === 0;

  return {
    ok,
    diagnostics: lint.diagnostics,
    output,
    stack,
    error
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

async function loadWasm(): Promise<WasmRuntimeExports> {
  if (!wasmPromise) {
    wasmPromise = (async () => {
      const response = await fetch(wasmPath());
      if (!response.ok) {
        throw new Error(`failed to fetch playground wasm (${response.status})`);
      }
      const bytes = await response.arrayBuffer();
      const { instance } = await WebAssembly.instantiate(bytes, {});
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
  invoke: (wasm: WasmRuntimeExports, sourcePtr: number, sourceLen: number, flavorPtr: number, flavorLen: number) => bigint,
  source: string,
  flavor: SourceFlavor
): Promise<unknown> {
  const wasm = await loadWasm();
  const sourceBytes = encoder.encode(source);
  const flavorBytes = encoder.encode(flavor);
  let sourcePtr = 0;
  let flavorPtr = 0;

  try {
    sourcePtr = writeBytes(wasm, sourceBytes);
    flavorPtr = writeBytes(wasm, flavorBytes);
    const packed = invoke(wasm, sourcePtr, sourceBytes.length, flavorPtr, flavorBytes.length);
    return decodeJsonPayload(wasm, packed);
  } finally {
    if (sourcePtr !== 0) {
      wasm.wasm_dealloc(sourcePtr, sourceBytes.length);
    }
    if (flavorPtr !== 0) {
      wasm.wasm_dealloc(flavorPtr, flavorBytes.length);
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

export async function lintWithWasm(source: string, flavor: SourceFlavor): Promise<LintReport> {
  const raw = await invokePackedJson(
    (wasm, sourcePtr, sourceLen, flavorPtr, flavorLen) =>
      wasm.lint_source_json(sourcePtr, sourceLen, flavorPtr, flavorLen),
    source,
    flavor
  );
  return normalizeLintReport(raw);
}

export async function runWithWasm(source: string, flavor: SourceFlavor): Promise<RunReport> {
  const raw = await invokePackedJson(
    (wasm, sourcePtr, sourceLen, flavorPtr, flavorLen) =>
      wasm.run_source_json(sourcePtr, sourceLen, flavorPtr, flavorLen),
    source,
    flavor
  );
  return normalizeRunReport(raw);
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
