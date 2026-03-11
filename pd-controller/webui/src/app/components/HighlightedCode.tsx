import { useCallback, useEffect, useRef } from "react";
import Editor, { type OnMount } from "@monaco-editor/react";
import type * as Monaco from "monaco-editor";

import { monacoLanguageForFlavor } from "@/app/helpers";
import { lintWithWasm } from "@/app/lint/wasmLinter";
import { LINT_MARKER_OWNER, lintFailureMarker, lintMarkersFromReport } from "@/app/monaco/lintMarkers";
import { ensureCompletionCatalogProviders, lookupCallableHover } from "@/app/monaco/completionCatalog";
import { ensureFormattingProviders } from "@/app/monaco/formatting";
import { lookupLocalTypeHover } from "@/app/monaco/localTypeHover";
import { ensureRustScriptLanguage } from "@/app/monaco/rustscriptLanguage";
import type { SourceFlavor, UiSourceBundle } from "@/app/types";

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
  const hoverProviderDisposableRef = useRef<Monaco.IDisposable | null>(null);
  const lintSeqRef = useRef(0);
  const lintTimeoutRef = useRef<number | null>(null);
  const currentCodeRef = useRef(code);
  const currentFlavorRef = useRef(flavor);

  currentCodeRef.current = code;
  currentFlavorRef.current = flavor;

  const clearLintMarkers = useCallback(() => {
    const editor = editorRef.current;
    const monaco = monacoRef.current;
    const model = editor?.getModel();
    if (!editor || !monaco || !model) {
      return;
    }
    monaco.editor.setModelMarkers(model, LINT_MARKER_OWNER, []);
  }, []);

  const onEditorMount: OnMount = useCallback((editor, monaco) => {
    ensureRustScriptLanguage(monaco);
    void ensureCompletionCatalogProviders(monaco);
    ensureFormattingProviders(monaco);
    editorRef.current = editor;
    monacoRef.current = monaco;
    if (!(enableLint && !readOnly)) {
      const model = editor.getModel();
      if (model) {
        monaco.editor.setModelMarkers(model, LINT_MARKER_OWNER, []);
      }
    }
  }, [enableLint, readOnly]);

  const onBeforeMount = useCallback((monaco: typeof import("monaco-editor")) => {
    ensureRustScriptLanguage(monaco);
    void ensureCompletionCatalogProviders(monaco);
    ensureFormattingProviders(monaco);
  }, []);

  useEffect(() => {
    const editor = editorRef.current;
    const monaco = monacoRef.current;
    const model = editor?.getModel();
    if (!editor || !monaco || !model) {
      return;
    }

    hoverProviderDisposableRef.current?.dispose();
    hoverProviderDisposableRef.current = monaco.languages.registerHoverProvider(model.getLanguageId(), {
      provideHover: async (hoverModel, position) => {
        const activeModel = editorRef.current?.getModel();
        if (!activeModel || hoverModel.uri.toString() !== activeModel.uri.toString()) {
          return null;
        }

        const callableHover = await lookupCallableHover(monaco, hoverModel, position);
        const compileTypeHover = await lookupLocalTypeHover(
          hoverModel,
          position,
          currentCodeRef.current,
          currentFlavorRef.current,
          `${hoverModel.uri.toString()}:${hoverModel.getVersionId()}`,
          monaco
        );
        if (callableHover && compileTypeHover) {
          return {
            range: callableHover.range ?? compileTypeHover.hover.range,
            contents: [...callableHover.contents, ...compileTypeHover.hover.contents]
          };
        }
        return callableHover ?? compileTypeHover?.hover ?? null;
      }
    });

    return () => {
      hoverProviderDisposableRef.current?.dispose();
      hoverProviderDisposableRef.current = null;
    };
  }, [flavor]);

  useEffect(() => {
    return () => {
      if (lintTimeoutRef.current !== null) {
        window.clearTimeout(lintTimeoutRef.current);
        lintTimeoutRef.current = null;
      }
      hoverProviderDisposableRef.current?.dispose();
      hoverProviderDisposableRef.current = null;
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
        const markers = lintMarkersFromReport(report, currentModel, monacoRef.current);
        monacoRef.current.editor.setModelMarkers(currentModel, LINT_MARKER_OWNER, markers);
      } catch (error) {
        if (currentSeq !== lintSeqRef.current) {
          return;
        }
        const currentModel = editorRef.current?.getModel();
        if (!currentModel || !monacoRef.current) {
          return;
        }
        const message = error instanceof Error ? error.message : "wasm linter failed";
        const markers = lintFailureMarker(message, currentModel, monacoRef.current);
        monacoRef.current.editor.setModelMarkers(currentModel, LINT_MARKER_OWNER, markers);
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
        beforeMount={onBeforeMount}
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
