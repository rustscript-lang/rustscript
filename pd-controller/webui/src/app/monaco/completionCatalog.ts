import { completionCatalogWithWasm, type CompletionCatalog, type CompletionEntry } from "@/app/lint/wasmLinter";

let registrationPromise: Promise<void> | null = null;
let registered = false;
let cachedCatalog: CompletionCatalog | null = null;

function entriesForLanguage(catalog: CompletionCatalog, languageId: string): CompletionEntry[] {
  if (languageId === "rustscript") {
    return catalog.rustscript;
  }
  if (languageId === "javascript") {
    return catalog.javascript;
  }
  if (languageId === "lua") {
    return catalog.lua;
  }
  if (languageId === "scheme") {
    return catalog.scheme;
  }
  return [];
}

function isCallableTokenChar(value: string): boolean {
  return /[A-Za-z0-9_:.]/.test(value);
}

function callableTokenAtPosition(
  model: import("monaco-editor").editor.ITextModel,
  position: import("monaco-editor").Position
): { token: string; range: import("monaco-editor").IRange } | null {
  const line = model.getLineContent(position.lineNumber);
  const offset = Math.max(0, position.column - 1);
  if (line.length === 0) {
    return null;
  }

  let start = Math.min(offset, Math.max(0, line.length - 1));
  let end = start;
  if (!isCallableTokenChar(line[start] ?? "")) {
    if (start > 0 && isCallableTokenChar(line[start - 1] ?? "")) {
      start -= 1;
      end = start;
    } else {
      return null;
    }
  }

  while (start > 0 && isCallableTokenChar(line[start - 1] ?? "")) {
    start -= 1;
  }
  while (end + 1 < line.length && isCallableTokenChar(line[end + 1] ?? "")) {
    end += 1;
  }

  const token = line.slice(start, end + 1);
  if (!token) {
    return null;
  }

  if (model.getLanguageId() === "scheme") {
    let prev = start - 1;
    while (prev >= 0 && /\s/.test(line[prev] ?? "")) {
      prev -= 1;
    }
    if (line[prev] !== "(") {
      return null;
    }
  } else {
    let next = end + 1;
    while (next < line.length && /\s/.test(line[next] ?? "")) {
      next += 1;
    }
    if (line[next] !== "(") {
      return null;
    }
  }

  return {
    token,
    range: {
      startLineNumber: position.lineNumber,
      startColumn: start + 1,
      endLineNumber: position.lineNumber,
      endColumn: end + 2
    }
  };
}

async function loadCompletionCatalog(): Promise<CompletionCatalog> {
  if (cachedCatalog) {
    return cachedCatalog;
  }
  const catalog = await completionCatalogWithWasm();
  cachedCatalog = catalog;
  return catalog;
}

function completionItemKind(
  monaco: typeof import("monaco-editor"),
  kind: CompletionEntry["kind"]
): import("monaco-editor").languages.CompletionItemKind {
  if (kind === "function") {
    return monaco.languages.CompletionItemKind.Function;
  }
  if (kind === "module") {
    return monaco.languages.CompletionItemKind.Module;
  }
  return monaco.languages.CompletionItemKind.Snippet;
}

function registerCompletionProvider(
  monaco: typeof import("monaco-editor"),
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
      const range: import("monaco-editor").IRange = {
        startLineNumber: position.lineNumber,
        endLineNumber: position.lineNumber,
        startColumn: word.startColumn,
        endColumn: word.endColumn
      };

      const suggestions = entries.map((entry, index) => ({
        label: entry.label,
        kind: completionItemKind(monaco, entry.kind),
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

function registerCatalogCompletions(
  monaco: typeof import("monaco-editor"),
  catalog: CompletionCatalog
): void {
  registerCompletionProvider(monaco, "rustscript", catalog.rustscript, [":", "!"]);
  registerCompletionProvider(monaco, "javascript", catalog.javascript, ["."]);
  registerCompletionProvider(monaco, "lua", catalog.lua, [".", ":"]);
  registerCompletionProvider(monaco, "scheme", catalog.scheme, ["(", "."]);

  for (const languageId of ["rustscript", "javascript", "lua", "scheme"] as const) {
    monaco.languages.registerHoverProvider(languageId, {
      async provideHover(model, position) {
        return lookupCallableHover(monaco, model, position);
      }
    });
  }
}

export async function lookupCallableHover(
  monaco: typeof import("monaco-editor"),
  model: import("monaco-editor").editor.ITextModel,
  position: import("monaco-editor").Position
): Promise<import("monaco-editor").languages.Hover | null> {
  const tokenInfo = callableTokenAtPosition(model, position);
  if (!tokenInfo) {
    return null;
  }

  const catalog = await loadCompletionCatalog();
  const entry = entriesForLanguage(catalog, model.getLanguageId()).find(
    (candidate) => candidate.kind === "function" && candidate.label === tokenInfo.token
  );
  if (!entry) {
    return null;
  }

  const contents: import("monaco-editor").IMarkdownString[] = [];
  if (entry.detail) {
    contents.push({ value: `\`\`\`text\n${entry.detail}\n\`\`\`` });
  }
  if (entry.documentation) {
    contents.push({ value: entry.documentation });
  }
  if (contents.length === 0) {
    return null;
  }

  return {
    range: new monaco.Range(
      tokenInfo.range.startLineNumber,
      tokenInfo.range.startColumn,
      tokenInfo.range.endLineNumber,
      tokenInfo.range.endColumn
    ),
    contents
  };
}

export function ensureCompletionCatalogProviders(
  monaco: typeof import("monaco-editor")
): Promise<void> {
  if (registered) {
    return Promise.resolve();
  }
  if (registrationPromise) {
    return registrationPromise;
  }

  registrationPromise = loadCompletionCatalog()
    .then((catalog) => {
      registerCatalogCompletions(monaco, catalog);
      registered = true;
    })
    .catch((error) => {
      const message = error instanceof Error ? error.message : "unknown completion catalog error";
      console.warn(`failed to load completion catalog: ${message}`);
      registrationPromise = null;
    });

  return registrationPromise;
}
