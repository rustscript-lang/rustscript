const fs = require("node:fs/promises");
const path = require("node:path");
const { TextDecoder, TextEncoder } = require("node:util");
const {
  CompletionItemKind,
  createConnection,
  DiagnosticSeverity,
  InsertTextFormat,
  MarkupKind,
  ProposedFeatures,
  SymbolKind,
  TextDocumentSyncKind,
  TextDocuments
} = require("vscode-languageserver/node");
const { TextDocument } = require("vscode-languageserver-textdocument");

const LANGUAGE_ID = "rustscript";
const FLAVOR = "rustscript";
const COMPLETION_LIMIT = 200;

const connection = createConnection(ProposedFeatures.all);
const documents = new TextDocuments(TextDocument);

const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8");

let wasmExports = null;
let wasmLoadPromise = null;
let completionCatalogPromise = null;
let completionEntries = fallbackCompletionEntries();
let completionLookup = buildCompletionLookup(completionEntries);

const latestDocumentVersion = new Map();

connection.onInitialize(() => {
  return {
    capabilities: {
      textDocumentSync: TextDocumentSyncKind.Incremental,
      completionProvider: {
        resolveProvider: false,
        triggerCharacters: [":", ".", "("]
      },
      hoverProvider: true,
      documentSymbolProvider: true
    }
  };
});

connection.onInitialized(() => {
  void warmupLanguageData();
});

documents.onDidOpen((event) => {
  queueValidation(event.document);
});

documents.onDidSave((event) => {
  queueValidation(event.document);
});

documents.onDidClose((event) => {
  latestDocumentVersion.delete(event.document.uri);
  connection.sendDiagnostics({
    uri: event.document.uri,
    diagnostics: []
  });
});

documents.onDidChangeContent((event) => {
  queueValidation(event.document);
});

connection.onCompletion(async (params) => {
  await ensureCompletionCatalogLoaded();

  const document = documents.get(params.textDocument.uri);
  if (!document) {
    return [];
  }

  const prefix = completionPrefix(document, params.position);
  const normalizedPrefix = prefix.toLowerCase();
  const items = [];

  for (let index = 0; index < completionEntries.length; index += 1) {
    const entry = completionEntries[index];
    if (
      normalizedPrefix.length > 0 &&
      !entry.label.toLowerCase().includes(normalizedPrefix) &&
      !entry.insertText.toLowerCase().includes(normalizedPrefix)
    ) {
      continue;
    }

    items.push({
      label: entry.label,
      kind: mapCompletionKind(entry.kind),
      insertText: entry.insertText,
      insertTextFormat: InsertTextFormat.Snippet,
      detail: entry.detail,
      documentation: entry.documentation || undefined,
      sortText: `${String(index).padStart(4, "0")}_${entry.label}`
    });

    if (items.length >= COMPLETION_LIMIT) {
      break;
    }
  }

  if (items.length === 0 && normalizedPrefix.length > 0) {
    for (let index = 0; index < completionEntries.length; index += 1) {
      const entry = completionEntries[index];
      items.push({
        label: entry.label,
        kind: mapCompletionKind(entry.kind),
        insertText: entry.insertText,
        insertTextFormat: InsertTextFormat.Snippet,
        detail: entry.detail,
        documentation: entry.documentation || undefined,
        sortText: `${String(index).padStart(4, "0")}_${entry.label}`
      });
      if (items.length >= Math.min(40, COMPLETION_LIMIT)) {
        break;
      }
    }
  }

  return items;
});

connection.onHover(async (params) => {
  await ensureCompletionCatalogLoaded();

  const document = documents.get(params.textDocument.uri);
  if (!document) {
    return null;
  }

  const token = tokenAt(document, params.position);
  if (!token) {
    return null;
  }

  const entry = lookupCompletionEntry(token.text);
  if (!entry) {
    return null;
  }

  const sections = [`\`${entry.label}\``];
  if (entry.detail) {
    sections.push(entry.detail);
  }
  if (entry.documentation) {
    sections.push(entry.documentation);
  }

  return {
    contents: {
      kind: MarkupKind.Markdown,
      value: sections.join("\n\n")
    },
    range: token.range
  };
});

connection.onDocumentSymbol((params) => {
  const document = documents.get(params.textDocument.uri);
  if (!document) {
    return [];
  }
  return collectDocumentSymbols(document);
});

documents.listen(connection);
connection.listen();

async function warmupLanguageData() {
  const wasm = await ensureWasmLoaded();
  if (!wasm) {
    connection.console.warn(
      "RustScript lint WASM was not loaded. Falling back to basic delimiter diagnostics."
    );
    return;
  }

  await ensureCompletionCatalogLoaded();

  for (const document of documents.all()) {
    queueValidation(document);
  }
}

async function ensureCompletionCatalogLoaded() {
  if (completionCatalogPromise) {
    await completionCatalogPromise;
    return;
  }

  completionCatalogPromise = (async () => {
    const wasm = await ensureWasmLoaded();
    if (!wasm) {
      return;
    }
    const catalog = await readCompletionCatalog(wasm);
    if (catalog.length > 0) {
      completionEntries = catalog;
      completionLookup = buildCompletionLookup(completionEntries);
    }
  })();

  await completionCatalogPromise;
}

function queueValidation(document) {
  latestDocumentVersion.set(document.uri, document.version);
  void validateDocument(document);
}

async function validateDocument(document) {
  const version = document.version;
  const diagnostics = await computeDiagnostics(document);
  if (latestDocumentVersion.get(document.uri) !== version) {
    return;
  }
  connection.sendDiagnostics({
    uri: document.uri,
    diagnostics
  });
}

async function computeDiagnostics(document) {
  const source = document.getText();
  const lintDiagnostics = await runWasmLint(source);
  if (lintDiagnostics) {
    return lintDiagnostics
      .map((diagnostic) => toLspDiagnostic(document, diagnostic))
      .filter((diagnostic) => diagnostic !== null);
  }
  return scanDelimiters(document);
}

function toLspDiagnostic(document, diagnostic) {
  if (!diagnostic || typeof diagnostic !== "object") {
    return null;
  }

  const message =
    typeof diagnostic.message === "string" && diagnostic.message.length > 0
      ? diagnostic.message
      : typeof diagnostic.rendered === "string" && diagnostic.rendered.length > 0
        ? diagnostic.rendered
        : "RustScript lint error";

  const span = toRangeFromWasmSpan(document, diagnostic.span);
  const line = Number.isFinite(diagnostic.line)
    ? Math.max(0, Math.trunc(diagnostic.line))
    : 0;

  return {
    severity: DiagnosticSeverity.Error,
    source: "rustscript-lsp",
    message,
    range: span || fullLineRange(document, line > 0 ? line - 1 : 0)
  };
}

function toRangeFromWasmSpan(document, span) {
  if (!span || typeof span !== "object") {
    return null;
  }

  const maxLine = Math.max(0, document.lineCount - 1);
  const startLine = clamp(
    (toPositiveInteger(span.start_line, 1) || 1) - 1,
    0,
    maxLine
  );
  const endLine = clamp(
    (toPositiveInteger(span.end_line, startLine + 1) || startLine + 1) - 1,
    startLine,
    maxLine
  );
  const startChar = clamp(
    (toPositiveInteger(span.start_col, 1) || 1) - 1,
    0,
    lineLength(document, startLine)
  );
  const rawEndChar =
    (toPositiveInteger(span.end_col, startChar + 1) || startChar + 1) - 1;
  const endChar = clamp(
    endLine === startLine ? Math.max(startChar + 1, rawEndChar) : rawEndChar,
    0,
    lineLength(document, endLine)
  );

  return {
    start: { line: startLine, character: startChar },
    end: { line: endLine, character: endChar }
  };
}

function fullLineRange(document, lineNumber) {
  const maxLine = Math.max(0, document.lineCount - 1);
  const line = clamp(lineNumber, 0, maxLine);
  return {
    start: { line, character: 0 },
    end: { line, character: lineLength(document, line) }
  };
}

function scanDelimiters(document) {
  const source = document.getText();
  const lines = source.split(/\r?\n/);
  const diagnostics = [];
  const stack = [];
  let inString = false;
  let inBlockComment = false;
  let blockCommentStart = null;
  let stringStart = null;

  for (let line = 0; line < lines.length; line += 1) {
    const text = lines[line];
    let inLineComment = false;

    for (let column = 0; column < text.length; column += 1) {
      const current = text[column];
      const next = column + 1 < text.length ? text[column + 1] : "";

      if (inLineComment) {
        break;
      }

      if (inString) {
        if (current === "\\") {
          column += 1;
          continue;
        }
        if (current === "\"") {
          inString = false;
          stringStart = null;
        }
        continue;
      }

      if (inBlockComment) {
        if (current === "*" && next === "/") {
          inBlockComment = false;
          blockCommentStart = null;
          column += 1;
        }
        continue;
      }

      if (current === "/" && next === "/") {
        inLineComment = true;
        continue;
      }

      if (current === "/" && next === "*") {
        inBlockComment = true;
        blockCommentStart = { line, column };
        column += 1;
        continue;
      }

      if (current === "\"") {
        inString = true;
        stringStart = { line, column };
        continue;
      }

      if (current === "{" || current === "(" || current === "[") {
        stack.push({
          char: current,
          line,
          column
        });
        continue;
      }

      if (current === "}" || current === ")" || current === "]") {
        const opener = stack.pop();
        if (!opener) {
          diagnostics.push({
            severity: DiagnosticSeverity.Error,
            source: "rustscript-lsp",
            message: `Unmatched closing delimiter '${current}'.`,
            range: {
              start: { line, character: column },
              end: { line, character: column + 1 }
            }
          });
          continue;
        }
        if (!delimitersMatch(opener.char, current)) {
          diagnostics.push({
            severity: DiagnosticSeverity.Error,
            source: "rustscript-lsp",
            message: `Delimiter '${opener.char}' does not match closing '${current}'.`,
            range: {
              start: { line, character: column },
              end: { line, character: column + 1 }
            }
          });
        }
      }
    }
  }

  while (stack.length > 0) {
    const opener = stack.pop();
    diagnostics.push({
      severity: DiagnosticSeverity.Error,
      source: "rustscript-lsp",
      message: `Unclosed delimiter '${opener.char}'.`,
      range: {
        start: { line: opener.line, character: opener.column },
        end: { line: opener.line, character: opener.column + 1 }
      }
    });
  }

  if (inBlockComment && blockCommentStart) {
    diagnostics.push({
      severity: DiagnosticSeverity.Error,
      source: "rustscript-lsp",
      message: "Unclosed block comment.",
      range: {
        start: { line: blockCommentStart.line, character: blockCommentStart.column },
        end: {
          line: blockCommentStart.line,
          character: blockCommentStart.column + 2
        }
      }
    });
  }

  if (inString && stringStart) {
    diagnostics.push({
      severity: DiagnosticSeverity.Error,
      source: "rustscript-lsp",
      message: "Unclosed string literal.",
      range: {
        start: { line: stringStart.line, character: stringStart.column },
        end: { line: stringStart.line, character: stringStart.column + 1 }
      }
    });
  }

  return diagnostics;
}

function delimitersMatch(openChar, closeChar) {
  return (
    (openChar === "{" && closeChar === "}") ||
    (openChar === "(" && closeChar === ")") ||
    (openChar === "[" && closeChar === "]")
  );
}

function collectDocumentSymbols(document) {
  const text = document.getText();
  const lines = text.split(/\r?\n/);
  const symbols = [];

  for (let line = 0; line < lines.length; line += 1) {
    const content = lines[line];

    const fnRegex = /\b(?:pub\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(/g;
    let fnMatch = fnRegex.exec(content);
    while (fnMatch) {
      const name = fnMatch[1];
      const startCharacter = fnMatch.index;
      symbols.push({
        name,
        kind: SymbolKind.Function,
        range: {
          start: { line, character: startCharacter },
          end: { line, character: content.length }
        },
        selectionRange: {
          start: { line, character: startCharacter },
          end: { line, character: startCharacter + name.length }
        }
      });
      fnMatch = fnRegex.exec(content);
    }

    const letRegex = /\blet\s+(?:mut\s+)?([A-Za-z_][A-Za-z0-9_]*)\b/g;
    let letMatch = letRegex.exec(content);
    while (letMatch) {
      const name = letMatch[1];
      const startCharacter = letMatch.index;
      symbols.push({
        name,
        kind: SymbolKind.Variable,
        range: {
          start: { line, character: startCharacter },
          end: { line, character: content.length }
        },
        selectionRange: {
          start: { line, character: startCharacter },
          end: { line, character: startCharacter + name.length }
        }
      });
      letMatch = letRegex.exec(content);
    }
  }

  return symbols;
}

function completionPrefix(document, position) {
  const token = tokenAt(document, position);
  return token ? token.text : "";
}

function tokenAt(document, position) {
  const line = lineText(document, position.line);
  if (!line) {
    return null;
  }

  let start = clamp(position.character, 0, line.length);
  let end = start;

  while (start > 0 && isTokenChar(line[start - 1])) {
    start -= 1;
  }
  while (end < line.length && isTokenChar(line[end])) {
    end += 1;
  }

  if (start === end) {
    return null;
  }

  return {
    text: line.slice(start, end),
    range: {
      start: { line: position.line, character: start },
      end: { line: position.line, character: end }
    }
  };
}

function lookupCompletionEntry(token) {
  if (!token) {
    return null;
  }

  const normalized = token.toLowerCase();
  if (completionLookup.has(normalized)) {
    return completionLookup.get(normalized);
  }

  for (const entry of completionEntries) {
    if (
      entry.label.toLowerCase().endsWith(normalized) ||
      entry.insertText.toLowerCase().startsWith(normalized)
    ) {
      return entry;
    }
  }

  return null;
}

function buildCompletionLookup(entries) {
  const lookup = new Map();
  for (const entry of entries) {
    const key = entry.label.toLowerCase();
    if (!lookup.has(key)) {
      lookup.set(key, entry);
    }
  }
  return lookup;
}

function mapCompletionKind(kind) {
  if (kind === "function") {
    return CompletionItemKind.Function;
  }
  if (kind === "module") {
    return CompletionItemKind.Module;
  }
  return CompletionItemKind.Snippet;
}

async function runWasmLint(source) {
  const wasm = await ensureWasmLoaded();
  if (!wasm) {
    return null;
  }

  const sourceBytes = encoder.encode(source);
  const flavorBytes = encoder.encode(FLAVOR);
  let sourcePtr = 0;
  let flavorPtr = 0;
  let resultPtr = 0;
  let resultLen = 0;

  try {
    sourcePtr = writeBytes(wasm, sourceBytes);
    flavorPtr = writeBytes(wasm, flavorBytes);
    const packed = wasm.lint_source_json(
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

    const resultBytes = readBytes(wasm, resultPtr, resultLen);
    const parsed = JSON.parse(decoder.decode(resultBytes));
    if (!parsed || !Array.isArray(parsed.diagnostics)) {
      return [];
    }
    return parsed.diagnostics;
  } catch (error) {
    connection.console.error(`RustScript lint failed: ${errorMessage(error)}`);
    return null;
  } finally {
    freeBytes(wasm, sourcePtr, sourceBytes.length);
    freeBytes(wasm, flavorPtr, flavorBytes.length);
    freeBytes(wasm, resultPtr, resultLen);
  }
}

async function readCompletionCatalog(wasm) {
  if (!wasm || typeof wasm.completion_catalog_json !== "function") {
    return [];
  }

  let ptr = 0;
  let len = 0;
  try {
    const packed = wasm.completion_catalog_json();
    const unpacked = unpackPtrLen(packed);
    ptr = unpacked.ptr;
    len = unpacked.len;
    if (ptr === 0 || len === 0) {
      return [];
    }

    const parsed = JSON.parse(decoder.decode(readBytes(wasm, ptr, len)));
    if (!parsed || !Array.isArray(parsed.rustscript)) {
      return [];
    }

    return parsed.rustscript
      .map(normalizeCompletionEntry)
      .filter((entry) => entry !== null);
  } catch (error) {
    connection.console.error(`RustScript completions failed: ${errorMessage(error)}`);
    return [];
  } finally {
    freeBytes(wasm, ptr, len);
  }
}

function normalizeCompletionEntry(raw) {
  if (!raw || typeof raw !== "object") {
    return null;
  }

  const label = typeof raw.label === "string" ? raw.label : "";
  const insertText =
    typeof raw.insert_text === "string" && raw.insert_text.length > 0
      ? raw.insert_text
      : label;

  if (!label) {
    return null;
  }

  return {
    label,
    insertText,
    detail: typeof raw.detail === "string" ? raw.detail : "",
    documentation:
      typeof raw.documentation === "string" ? raw.documentation : "",
    kind: typeof raw.kind === "string" ? raw.kind : "snippet"
  };
}

async function ensureWasmLoaded() {
  if (wasmExports) {
    return wasmExports;
  }

  if (!wasmLoadPromise) {
    wasmLoadPromise = (async () => {
      const wasmPath = resolveWasmPath();
      try {
        const bytes = await fs.readFile(wasmPath);
        const instantiated = await WebAssembly.instantiate(bytes, {});
        const instance = instantiated.instance;
        const exports = instance.exports;
        if (!isValidWasmExports(exports)) {
          connection.console.error(
            `RustScript lint WASM exports are invalid: ${wasmPath}`
          );
          return null;
        }

        connection.console.info(`Loaded RustScript lint WASM: ${wasmPath}`);
        wasmExports = exports;
        return wasmExports;
      } catch (error) {
        connection.console.error(
          `Unable to load RustScript lint WASM (${wasmPath}): ${errorMessage(error)}`
        );
        return null;
      }
    })();
  }

  const loaded = await wasmLoadPromise;
  if (!loaded) {
    wasmLoadPromise = null;
  }
  return loaded;
}

function resolveWasmPath() {
  const configured = (process.env.RUSTSCRIPT_LINT_WASM || "").trim();
  if (configured.length > 0) {
    return path.resolve(configured);
  }
  return path.resolve(__dirname, "..", "wasm", "pd_vm_lint_wasm.wasm");
}

function isValidWasmExports(exports) {
  return (
    exports &&
    exports.memory instanceof WebAssembly.Memory &&
    typeof exports.wasm_alloc === "function" &&
    typeof exports.wasm_dealloc === "function" &&
    typeof exports.lint_source_json === "function"
  );
}

function unpackPtrLen(packedValue) {
  const packed =
    typeof packedValue === "bigint" ? packedValue : BigInt(packedValue);
  const ptr = Number(packed & 0xffff_ffffn);
  const len = Number((packed >> 32n) & 0xffff_ffffn);
  return { ptr, len };
}

function writeBytes(wasm, bytes) {
  if (!bytes || bytes.length === 0) {
    return 0;
  }
  const ptr = wasm.wasm_alloc(bytes.length);
  const memory = new Uint8Array(wasm.memory.buffer);
  memory.set(bytes, ptr);
  return ptr;
}

function readBytes(wasm, ptr, len) {
  return new Uint8Array(wasm.memory.buffer, ptr, len);
}

function freeBytes(wasm, ptr, len) {
  if (!ptr || !len) {
    return;
  }
  wasm.wasm_dealloc(ptr, len);
}

function lineText(document, lineNumber) {
  if (lineNumber < 0 || lineNumber >= document.lineCount) {
    return "";
  }
  const start = document.offsetAt({ line: lineNumber, character: 0 });
  const end =
    lineNumber + 1 < document.lineCount
      ? document.offsetAt({ line: lineNumber + 1, character: 0 })
      : document.getText().length;
  return document
    .getText()
    .slice(start, end)
    .replace(/\r?\n$/, "");
}

function lineLength(document, lineNumber) {
  return lineText(document, lineNumber).length;
}

function isTokenChar(value) {
  return /[A-Za-z0-9_:.]/.test(value);
}

function clamp(value, min, max) {
  if (value < min) {
    return min;
  }
  if (value > max) {
    return max;
  }
  return value;
}

function toPositiveInteger(value, fallback) {
  if (!Number.isFinite(value)) {
    return fallback;
  }
  const parsed = Math.trunc(value);
  if (parsed <= 0) {
    return fallback;
  }
  return parsed;
}

function errorMessage(error) {
  if (error instanceof Error) {
    return error.message;
  }
  return String(error);
}

function fallbackCompletionEntries() {
  const keywords = [
    "pub",
    "fn",
    "let",
    "mut",
    "for",
    "if",
    "else",
    "match",
    "while",
    "break",
    "continue",
    "use",
    "as",
    "true",
    "false",
    "null"
  ];
  const snippets = [
    {
      label: "fn",
      insertText: "fn ${1:name}(${2:args}) {\n    $0\n}",
      detail: "Function declaration",
      documentation: "Declare a RustScript function.",
      kind: "snippet"
    },
    {
      label: "if",
      insertText: "if ${1:condition} {\n    $0\n}",
      detail: "If statement",
      documentation: "Conditional branch.",
      kind: "snippet"
    },
    {
      label: "while",
      insertText: "while ${1:condition} {\n    $0\n}",
      detail: "While loop",
      documentation: "Repeat while condition is true.",
      kind: "snippet"
    },
    {
      label: "use vm;",
      insertText: "use vm;",
      detail: "Host import",
      documentation: "Import the `vm` namespace for host function access.",
      kind: "module"
    }
  ];

  const keywordEntries = keywords.map((keyword) => ({
    label: keyword,
    insertText: keyword,
    detail: "Keyword",
    documentation: `RustScript keyword \`${keyword}\`.`,
    kind: "snippet"
  }));

  return [...snippets, ...keywordEntries];
}
