import type { SourceFlavor } from "@/app/types";

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

type WasmLinterExports = {
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
  completion_catalog_json?: () => bigint;
};

const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8");
let wasmPromise: Promise<WasmLinterExports> | null = null;

function wasmPath(): string {
  const base = import.meta.env.BASE_URL ?? "/";
  return `${base.replace(/\/+$/, "/")}wasm/pd_vm_wasm.wasm`;
}

function writeBytes(wasm: WasmLinterExports, bytes: Uint8Array): number {
  const ptr = wasm.wasm_alloc(bytes.length);
  const memory = new Uint8Array(wasm.memory.buffer);
  memory.set(bytes, ptr);
  return ptr;
}

function readBytes(wasm: WasmLinterExports, ptr: number, len: number): Uint8Array {
  return new Uint8Array(wasm.memory.buffer, ptr, len);
}

function unpackPtrLen(packed: bigint): { ptr: number; len: number } {
  const ptr = Number(packed & 0xFFFF_FFFFn);
  const len = Number((packed >> 32n) & 0xFFFF_FFFFn);
  return { ptr, len };
}

function normalizeReport(parsed: unknown): LintReport {
  if (!parsed || typeof parsed !== "object" || !("diagnostics" in parsed)) {
    return { diagnostics: [] };
  }
  const diagnosticsRaw = (parsed as { diagnostics?: unknown }).diagnostics;
  if (!Array.isArray(diagnosticsRaw)) {
    return { diagnostics: [] };
  }
  const diagnostics: LintDiagnostic[] = [];
  for (const item of diagnosticsRaw) {
    if (!item || typeof item !== "object") {
      continue;
    }
    const lineRaw = Number((item as { line?: unknown }).line);
    const severityRaw = (item as { severity?: unknown }).severity;
    const messageRaw = (item as { message?: unknown }).message;
    const renderedRaw = (item as { rendered?: unknown }).rendered;
    let span: LintSpan | null = null;
    const spanRaw = (item as { span?: unknown }).span;
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
    diagnostics.push({
      line,
      severity,
      message,
      span,
      rendered
    });
  }
  return { diagnostics };
}

function normalizeFormatReport(parsed: unknown): FormatReport {
  if (!parsed || typeof parsed !== "object") {
    return {
      ok: false,
      formatted: null,
      error: "invalid format response"
    };
  }
  const rawOk = (parsed as { ok?: unknown }).ok;
  const rawFormatted = (parsed as { formatted?: unknown }).formatted;
  const rawError = (parsed as { error?: unknown }).error;
  return {
    ok: typeof rawOk === "boolean" ? rawOk : typeof rawFormatted === "string",
    formatted: typeof rawFormatted === "string" ? rawFormatted : null,
    error: typeof rawError === "string" ? rawError : null
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

async function loadWasm(): Promise<WasmLinterExports> {
  if (!wasmPromise) {
    wasmPromise = (async () => {
      const response = await fetch(wasmPath());
      if (!response.ok) {
        throw new Error(`failed to fetch wasm linter (${response.status})`);
      }
      const bytes = await response.arrayBuffer();
      const { instance } = await WebAssembly.instantiate(bytes, {});
      const exports = instance.exports as Partial<WasmLinterExports>;
      if (
        !exports.memory ||
        typeof exports.wasm_alloc !== "function" ||
        typeof exports.wasm_dealloc !== "function" ||
        typeof exports.lint_source_json !== "function"
      ) {
        throw new Error("invalid wasm linter exports");
      }
      return exports as WasmLinterExports;
    })();
  }
  return wasmPromise;
}

export async function lintWithWasm(source: string, flavor: SourceFlavor): Promise<LintReport> {
  const wasm = await loadWasm();
  const sourceBytes = encoder.encode(source);
  const flavorBytes = encoder.encode(flavor);
  let sourcePtr = 0;
  let flavorPtr = 0;
  let resultPtr = 0;
  let resultLen = 0;

  try {
    sourcePtr = writeBytes(wasm, sourceBytes);
    flavorPtr = writeBytes(wasm, flavorBytes);
    const packed = wasm.lint_source_json(sourcePtr, sourceBytes.length, flavorPtr, flavorBytes.length);
    const unpacked = unpackPtrLen(packed);
    resultPtr = unpacked.ptr;
    resultLen = unpacked.len;
    if (resultPtr === 0 || resultLen === 0) {
      return { diagnostics: [] };
    }
    const json = decoder.decode(readBytes(wasm, resultPtr, resultLen));
    return normalizeReport(JSON.parse(json));
  } finally {
    if (sourcePtr !== 0) {
      wasm.wasm_dealloc(sourcePtr, sourceBytes.length);
    }
    if (flavorPtr !== 0) {
      wasm.wasm_dealloc(flavorPtr, flavorBytes.length);
    }
    if (resultPtr !== 0 && resultLen > 0) {
      wasm.wasm_dealloc(resultPtr, resultLen);
    }
  }
}

export async function formatWithWasm(
  source: string,
  flavor: SourceFlavor
): Promise<FormatReport> {
  const wasm = await loadWasm();
  if (typeof wasm.format_source_json !== "function") {
    return {
      ok: false,
      formatted: null,
      error: "formatting wasm export is unavailable"
    };
  }

  const sourceBytes = encoder.encode(source);
  const flavorBytes = encoder.encode(flavor);
  let sourcePtr = 0;
  let flavorPtr = 0;
  let resultPtr = 0;
  let resultLen = 0;

  try {
    sourcePtr = writeBytes(wasm, sourceBytes);
    flavorPtr = writeBytes(wasm, flavorBytes);
    const packed = wasm.format_source_json(
      sourcePtr,
      sourceBytes.length,
      flavorPtr,
      flavorBytes.length
    );
    const unpacked = unpackPtrLen(packed);
    resultPtr = unpacked.ptr;
    resultLen = unpacked.len;
    if (resultPtr === 0 || resultLen === 0) {
      return {
        ok: false,
        formatted: null,
        error: "empty format response"
      };
    }
    const json = decoder.decode(readBytes(wasm, resultPtr, resultLen));
    return normalizeFormatReport(JSON.parse(json));
  } finally {
    if (sourcePtr !== 0) {
      wasm.wasm_dealloc(sourcePtr, sourceBytes.length);
    }
    if (flavorPtr !== 0) {
      wasm.wasm_dealloc(flavorPtr, flavorBytes.length);
    }
    if (resultPtr !== 0 && resultLen > 0) {
      wasm.wasm_dealloc(resultPtr, resultLen);
    }
  }
}

export async function completionCatalogWithWasm(): Promise<CompletionCatalog> {
  const wasm = await loadWasm();
  if (typeof wasm.completion_catalog_json !== "function") {
    return emptyCompletionCatalog();
  }

  const packed = wasm.completion_catalog_json();
  const { ptr, len } = unpackPtrLen(packed);
  if (ptr === 0 || len === 0) {
    return emptyCompletionCatalog();
  }

  try {
    const json = decoder.decode(readBytes(wasm, ptr, len));
    return normalizeCompletionCatalog(JSON.parse(json));
  } finally {
    wasm.wasm_dealloc(ptr, len);
  }
}

export async function localTypeHintsWithWasm(
  source: string,
  flavor: SourceFlavor
): Promise<LocalTypeHint[]> {
  const wasm = await loadWasm();
  if (typeof wasm.local_type_hints_json !== "function") {
    return [];
  }

  const sourceBytes = encoder.encode(source);
  const flavorBytes = encoder.encode(flavor);
  let sourcePtr = 0;
  let flavorPtr = 0;
  let resultPtr = 0;
  let resultLen = 0;

  try {
    sourcePtr = writeBytes(wasm, sourceBytes);
    flavorPtr = writeBytes(wasm, flavorBytes);
    const packed = wasm.local_type_hints_json(
      sourcePtr,
      sourceBytes.length,
      flavorPtr,
      flavorBytes.length
    );
    const unpacked = unpackPtrLen(packed);
    resultPtr = unpacked.ptr;
    resultLen = unpacked.len;
    if (resultPtr === 0 || resultLen === 0) {
      return [];
    }
    const json = decoder.decode(readBytes(wasm, resultPtr, resultLen));
    return normalizeLocalTypeHints(JSON.parse(json));
  } finally {
    if (sourcePtr !== 0) {
      wasm.wasm_dealloc(sourcePtr, sourceBytes.length);
    }
    if (flavorPtr !== 0) {
      wasm.wasm_dealloc(flavorPtr, flavorBytes.length);
    }
    if (resultPtr !== 0 && resultLen > 0) {
      wasm.wasm_dealloc(resultPtr, resultLen);
    }
  }
}
