import { completionCatalogWithWasm, type CompletionCatalog, type CompletionEntry } from "@/app/lint/wasmLinter";

let registrationPromise: Promise<void> | null = null;
let registered = false;

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

  registrationPromise = completionCatalogWithWasm()
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
