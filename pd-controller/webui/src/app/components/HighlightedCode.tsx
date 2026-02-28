import { useCallback, useEffect, useRef } from "react";
import Editor, { type OnMount } from "@monaco-editor/react";
import type * as Monaco from "monaco-editor";

import { monacoLanguageForFlavor } from "@/app/helpers";
import { lintWithWasm } from "@/app/lint/wasmLinter";
import type { SourceFlavor, UiSourceBundle } from "@/app/types";

const LINT_OWNER = "pd-vm-wasm-lint";

export function HighlightedCode({
  flavor,
  source,
  readOnly = true,
  height = "520px",
  onChange,
  enableLint = false
}: {
  flavor: SourceFlavor;
  source: UiSourceBundle;
  readOnly?: boolean;
  height?: string;
  onChange?: (value: string) => void;
  enableLint?: boolean;
}) {
  const language = monacoLanguageForFlavor(flavor);
  const code = source[flavor] ?? "";
  const editorRef = useRef<Monaco.editor.IStandaloneCodeEditor | null>(null);
  const monacoRef = useRef<typeof import("monaco-editor") | null>(null);
  const lintSeqRef = useRef(0);
  const lintTimeoutRef = useRef<number | null>(null);

  const clearLintMarkers = useCallback(() => {
    const editor = editorRef.current;
    const monaco = monacoRef.current;
    const model = editor?.getModel();
    if (!editor || !monaco || !model) {
      return;
    }
    monaco.editor.setModelMarkers(model, LINT_OWNER, []);
  }, []);

  const onEditorMount: OnMount = useCallback((editor, monaco) => {
    editorRef.current = editor;
    monacoRef.current = monaco;
    if (!(enableLint && !readOnly)) {
      const model = editor.getModel();
      if (model) {
        monaco.editor.setModelMarkers(model, LINT_OWNER, []);
      }
    }
  }, [enableLint, readOnly]);

  useEffect(() => {
    return () => {
      if (lintTimeoutRef.current !== null) {
        window.clearTimeout(lintTimeoutRef.current);
        lintTimeoutRef.current = null;
      }
      clearLintMarkers();
    };
  }, [clearLintMarkers]);

  useEffect(() => {
    const lintEnabled = enableLint && !readOnly;
    const editor = editorRef.current;
    const monaco = monacoRef.current;
    const model = editor?.getModel();

    if (!lintEnabled || !editor || !monaco || !model) {
      clearLintMarkers();
      return;
    }

    lintSeqRef.current += 1;
    const currentSeq = lintSeqRef.current;
    if (lintTimeoutRef.current !== null) {
      window.clearTimeout(lintTimeoutRef.current);
    }

    lintTimeoutRef.current = window.setTimeout(async () => {
      try {
        const report = await lintWithWasm(code, flavor);
        if (currentSeq !== lintSeqRef.current) {
          return;
        }
        const currentModel = editorRef.current?.getModel();
        if (!currentModel || !monacoRef.current) {
          return;
        }
        const markers: Monaco.editor.IMarkerData[] = report.diagnostics.map((item) => {
          const maxLine = Math.max(1, currentModel.getLineCount());
          const line = Math.min(Math.max(item.line || 1, 1), maxLine);
          return {
            severity: monacoRef.current!.MarkerSeverity.Error,
            message: item.message,
            startLineNumber: line,
            startColumn: 1,
            endLineNumber: line,
            endColumn: Math.max(2, currentModel.getLineMaxColumn(line))
          };
        });
        monacoRef.current.editor.setModelMarkers(currentModel, LINT_OWNER, markers);
      } catch (error) {
        if (currentSeq !== lintSeqRef.current) {
          return;
        }
        const currentModel = editorRef.current?.getModel();
        if (!currentModel || !monacoRef.current) {
          return;
        }
        const message = error instanceof Error ? error.message : "wasm linter failed";
        monacoRef.current.editor.setModelMarkers(currentModel, LINT_OWNER, [
          {
            severity: monacoRef.current.MarkerSeverity.Warning,
            message,
            startLineNumber: 1,
            startColumn: 1,
            endLineNumber: 1,
            endColumn: Math.max(2, currentModel.getLineMaxColumn(1))
          }
        ]);
      }
    }, 120);

    return () => {
      if (lintTimeoutRef.current !== null) {
        window.clearTimeout(lintTimeoutRef.current);
        lintTimeoutRef.current = null;
      }
    };
  }, [code, enableLint, flavor, readOnly, clearLintMarkers]);

  return (
    <div className="h-full overflow-auto rounded-md border border-border">
      <Editor
        height={height}
        language={language}
        value={code}
        theme="vs"
        onMount={onEditorMount}
        onChange={(value) => {
          if (onChange) {
            onChange(value ?? "");
          }
        }}
        options={{
          readOnly,
          minimap: { enabled: false },
          scrollBeyondLastLine: false,
          automaticLayout: true,
          wordWrap: "on",
          fontSize: 13,
          lineNumbersMinChars: 3
        }}
      />
    </div>
  );
}
