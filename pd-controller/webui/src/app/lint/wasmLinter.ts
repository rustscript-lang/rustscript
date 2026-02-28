import type { SourceFlavor } from "@/app/types";

type LintDiagnostic = {
  line: number;
  message: string;
};

type LintReport = {
  diagnostics: LintDiagnostic[];
};

type WasmLinterExports = {
  memory: WebAssembly.Memory;
  wasm_alloc(len: number): number;
  wasm_dealloc(ptr: number, len: number): void;
  lint_source_json(sourcePtr: number, sourceLen: number, flavorPtr: number, flavorLen: number): bigint;
};

const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8");
let wasmPromise: Promise<WasmLinterExports> | null = null;

function wasmPath(): string {
  const base = import.meta.env.BASE_URL ?? "/";
  return `${base.replace(/\/+$/, "/")}wasm/pd_vm_lint_wasm.wasm`;
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
    const line = Number((item as { line?: unknown }).line);
    const message = (item as { message?: unknown }).message;
    if (!Number.isFinite(line) || typeof message !== "string") {
      continue;
    }
    diagnostics.push({
      line: Math.max(0, Math.trunc(line)),
      message
    });
  }
  return { diagnostics };
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
