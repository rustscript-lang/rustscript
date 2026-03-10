import type * as Monaco from "monaco-editor";

import type { LintReport } from "@/app/lint/wasmLinter";

export const LINT_MARKER_OWNER = "pd-vm-wasm-lint";

export function lintMarkersFromReport(
  report: LintReport,
  model: Monaco.editor.ITextModel,
  monaco: typeof import("monaco-editor")
): Monaco.editor.IMarkerData[] {
  return report.diagnostics.map((item) => {
    const maxLine = Math.max(1, model.getLineCount());
    const fallbackLine = Math.min(Math.max(item.line || 1, 1), maxLine);
    const rawRange = item.span
      ? {
          startLineNumber: item.span.startLine,
          startColumn: item.span.startColumn,
          endLineNumber: item.span.endLine,
          endColumn: item.span.endColumn
        }
      : {
          startLineNumber: fallbackLine,
          startColumn: 1,
          endLineNumber: fallbackLine,
          endColumn: Math.max(2, model.getLineMaxColumn(fallbackLine))
        };
    const range = model.validateRange(rawRange);
    return {
      severity:
        item.severity === "warning" ? monaco.MarkerSeverity.Warning : monaco.MarkerSeverity.Error,
      message: item.message,
      startLineNumber: range.startLineNumber,
      startColumn: range.startColumn,
      endLineNumber: range.endLineNumber,
      endColumn: range.endColumn
    };
  });
}

export function lintFailureMarker(
  message: string,
  model: Monaco.editor.ITextModel,
  monaco: typeof import("monaco-editor")
): Monaco.editor.IMarkerData[] {
  return [
    {
      severity: monaco.MarkerSeverity.Warning,
      message,
      startLineNumber: 1,
      startColumn: 1,
      endLineNumber: 1,
      endColumn: Math.max(2, model.getLineMaxColumn(1))
    }
  ];
}
