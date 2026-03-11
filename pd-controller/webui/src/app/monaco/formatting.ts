import { formatWithWasm } from "@/app/lint/wasmLinter";
import type { SourceFlavor } from "@/app/types";

let registered = false;

function registerFormattingProvider(
  monaco: typeof import("monaco-editor"),
  languageId: string,
  flavor: SourceFlavor
): void {
  monaco.languages.registerDocumentFormattingEditProvider(languageId, {
    async provideDocumentFormattingEdits(model) {
      const source = model.getValue();
      const report = await formatWithWasm(source, flavor);
      if (!report.ok || report.formatted === null) {
        const detail = report.error ?? "unknown formatter error";
        console.warn(`formatting failed for ${languageId}: ${detail}`);
        return [];
      }
      if (report.formatted === source) {
        return [];
      }
      return [
        {
          range: model.getFullModelRange(),
          text: report.formatted
        }
      ];
    }
  });
}

export function ensureFormattingProviders(
  monaco: typeof import("monaco-editor")
): void {
  if (registered) {
    return;
  }
  registerFormattingProvider(monaco, "rustscript", "rustscript");
  registerFormattingProvider(monaco, "javascript", "javascript");
  registered = true;
}
