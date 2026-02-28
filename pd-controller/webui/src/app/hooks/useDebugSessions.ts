import { type OnMount } from "@monaco-editor/react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type * as Monaco from "monaco-editor";

import { looksLikeIdentifier } from "@/app/helpers";
import type {
  DebugCommandRequest,
  DebugCommandResponse,
  DebugSessionDetail,
  DebugSessionListResponse,
  DebugSessionSummary,
  EdgeSummary,
  RunDebugCommandFn,
  RunDebugCommandOptions
} from "@/app/types";

type UseDebugSessionsArgs = {
  onError: (message: string) => void;
  edgeSummaries: EdgeSummary[];
  showDebugSessionsSection: () => void;
};

export function useDebugSessions({ onError, edgeSummaries, showDebugSessionsSection }: UseDebugSessionsArgs) {
  const [debugView, setDebugView] = useState<"list" | "detail">("list");
  const [debugSessions, setDebugSessions] = useState<DebugSessionSummary[]>([]);
  const [selectedDebugSessionId, setSelectedDebugSessionId] = useState<string | null>(null);
  const [selectedDebugSession, setSelectedDebugSession] = useState<DebugSessionDetail | null>(null);
  const [debugEdgeId, setDebugEdgeId] = useState<string>("");
  const [debugMode, setDebugMode] = useState<"interactive" | "recording">("interactive");
  const [debugHeaderName, setDebugHeaderName] = useState<string>("x-pd-debug-nonce");
  const [debugRequestPath, setDebugRequestPath] = useState<string>("");
  const [debugRecordCount, setDebugRecordCount] = useState<string>("1");
  const [debugCreating, setDebugCreating] = useState(false);
  const [debugCommandLoading, setDebugCommandLoading] = useState(false);
  const [debugHoveredVar, setDebugHoveredVar] = useState<string>("");
  const [debugHoverValue, setDebugHoverValue] = useState<string>("");
  const [debugEditorReadyTick, setDebugEditorReadyTick] = useState(0);

  const debugEditorRef = useRef<Monaco.editor.IStandaloneCodeEditor | null>(null);
  const debugMonacoRef = useRef<typeof import("monaco-editor") | null>(null);
  const debugDecorationIdsRef = useRef<string[]>([]);
  const debugHoverProviderDisposableRef = useRef<Monaco.IDisposable | null>(null);
  const debugMouseMoveDisposableRef = useRef<Monaco.IDisposable | null>(null);
  const debugMouseLeaveDisposableRef = useRef<Monaco.IDisposable | null>(null);
  const selectedDebugSessionRef = useRef<DebugSessionDetail | null>(null);
  const debugCommandLoadingRef = useRef(false);
  const runDebugCommandRef = useRef<RunDebugCommandFn | null>(null);
  const debugHoverCacheRef = useRef<Map<string, string>>(new Map());
  const debugHoverInflightRef = useRef<Map<string, Promise<string | null>>>(new Map());
  const debugHoverActiveKeyRef = useRef<string>("");

  const loadDebugSessions = useCallback(async () => {
    const response = await fetch("/v1/debug-sessions");
    if (!response.ok) {
      throw new Error(`failed to load debug sessions (${response.status})`);
    }
    const data = (await response.json()) as DebugSessionListResponse;
    setDebugSessions(data.sessions);
  }, []);

  const loadDebugSessionDetail = useCallback(async (sessionId: string) => {
    const response = await fetch(`/v1/debug-sessions/${sessionId}`);
    if (!response.ok) {
      throw new Error(`failed to load debug session (${response.status})`);
    }
    const detail = (await response.json()) as DebugSessionDetail;
    setSelectedDebugSession(detail);
    setSelectedDebugSessionId(detail.session_id);
  }, []);

  useEffect(() => {
    if (!debugEdgeId && edgeSummaries.length > 0) {
      setDebugEdgeId(edgeSummaries[0].edge_id);
    }
  }, [debugEdgeId, edgeSummaries]);

  const selectDebugSession = useCallback(
    async (sessionId: string) => {
      onError("");
      try {
        await loadDebugSessionDetail(sessionId);
        setDebugView("detail");
        showDebugSessionsSection();
      } catch (err) {
        onError(err instanceof Error ? err.message : "failed to load debug session");
      }
    },
    [loadDebugSessionDetail, onError, showDebugSessionsSection]
  );

  const createDebugSession = useCallback(async () => {
    if (!debugEdgeId) {
      onError("select an edge for debug session");
      return;
    }
    const recordCount = Number.parseInt(debugRecordCount, 10);
    if (debugMode === "recording") {
      if (!debugRequestPath.trim()) {
        onError("recording mode requires a request path");
        return;
      }
      if (!Number.isFinite(recordCount) || recordCount < 1) {
        onError("record count must be >= 1");
        return;
      }
    }
    setDebugCreating(true);
    onError("");
    try {
      const response = await fetch("/v1/debug-sessions", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          edge_id: debugEdgeId,
          mode: debugMode,
          header_name: debugMode === "interactive" ? debugHeaderName.trim() || undefined : undefined,
          stop_on_entry: true,
          request_path: debugMode === "recording" ? debugRequestPath.trim() : undefined,
          record_count: debugMode === "recording" ? recordCount : undefined
        })
      });
      if (!response.ok) {
        throw new Error(await response.text());
      }
      const detail = (await response.json()) as DebugSessionDetail;
      await loadDebugSessions();
      await loadDebugSessionDetail(detail.session_id);
      setDebugView("detail");
      showDebugSessionsSection();
    } catch (err) {
      onError(err instanceof Error ? err.message : "failed to create debug session");
    } finally {
      setDebugCreating(false);
    }
  }, [
    debugEdgeId,
    debugHeaderName,
    debugMode,
    debugRecordCount,
    debugRequestPath,
    loadDebugSessionDetail,
    loadDebugSessions,
    onError,
    showDebugSessionsSection
  ]);

  const stopDebugSession = useCallback(async (sessionId?: string) => {
    const targetSessionId = sessionId ?? selectedDebugSessionId;
    if (!targetSessionId) {
      return;
    }
    setDebugCommandLoading(true);
    onError("");
    try {
      const response = await fetch(`/v1/debug-sessions/${targetSessionId}/stop`, {
        method: "POST"
      });
      if (!response.ok) {
        throw new Error(await response.text());
      }
      await Promise.all([loadDebugSessions(), loadDebugSessionDetail(targetSessionId)]);
    } catch (err) {
      onError(err instanceof Error ? err.message : "failed to stop debug session");
    } finally {
      setDebugCommandLoading(false);
    }
  }, [loadDebugSessionDetail, loadDebugSessions, onError, selectedDebugSessionId]);

  const deleteDebugSession = useCallback(async (sessionId?: string) => {
    const targetSessionId = sessionId ?? selectedDebugSessionId;
    if (!targetSessionId) {
      return;
    }
    setDebugCommandLoading(true);
    onError("");
    try {
      const response = await fetch(`/v1/debug-sessions/${targetSessionId}`, {
        method: "DELETE"
      });
      if (!response.ok) {
        throw new Error(await response.text());
      }
      if (selectedDebugSessionId === targetSessionId) {
        setSelectedDebugSession(null);
        setSelectedDebugSessionId(null);
        setDebugView("list");
      }
      await loadDebugSessions();
    } catch (err) {
      onError(err instanceof Error ? err.message : "failed to delete debug session");
    } finally {
      setDebugCommandLoading(false);
    }
  }, [loadDebugSessions, onError, selectedDebugSessionId]);

  const runDebugCommand = useCallback(
    async (request: DebugCommandRequest, options: RunDebugCommandOptions = {}) => {
      if (!selectedDebugSessionId) {
        return null;
      }
      const { silent = false, refresh = true } = options;
      if (!silent) {
        setDebugCommandLoading(true);
      }
      try {
        const response = await fetch(`/v1/debug-sessions/${selectedDebugSessionId}/command`, {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify(request)
        });
        if (!response.ok) {
          throw new Error(await response.text());
        }
        const result = (await response.json()) as DebugCommandResponse;
        if (refresh) {
          await Promise.all([loadDebugSessions(), loadDebugSessionDetail(selectedDebugSessionId)]);
        }
        return result;
      } catch (err) {
        if (!silent) {
          onError(err instanceof Error ? err.message : "failed to run debugger command");
        }
        return null;
      } finally {
        if (!silent) {
          setDebugCommandLoading(false);
        }
      }
    },
    [loadDebugSessionDetail, loadDebugSessions, onError, selectedDebugSessionId]
  );

  const selectRecording = useCallback(
    async (recordingId: string) => {
      if (!recordingId) {
        return;
      }
      await runDebugCommand({ kind: "select_recording", recording_id: recordingId });
    },
    [runDebugCommand]
  );

  useEffect(() => {
    selectedDebugSessionRef.current = selectedDebugSession;
  }, [selectedDebugSession]);

  useEffect(() => {
    debugCommandLoadingRef.current = debugCommandLoading;
  }, [debugCommandLoading]);

  useEffect(() => {
    runDebugCommandRef.current = runDebugCommand;
  }, [runDebugCommand]);

  const resolveDebugHoverValue = useCallback(async (session: DebugSessionDetail, variable: string) => {
    const cacheKey = `${session.session_id}:${variable}:${session.current_line ?? 0}`;
    const cached = debugHoverCacheRef.current.get(cacheKey);
    if (cached) {
      return cached;
    }

    let inflight = debugHoverInflightRef.current.get(cacheKey);
    if (!inflight) {
      inflight = (async () => {
        const result = await runDebugCommandRef.current?.(
          { kind: "print_var", name: variable },
          { silent: true, refresh: false }
        );
        if (!result) {
          return null;
        }
        const value = result.output.trim() || "(no value)";
        debugHoverCacheRef.current.set(cacheKey, value);
        return value;
      })();
      debugHoverInflightRef.current.set(cacheKey, inflight);
      void inflight.finally(() => {
        if (debugHoverInflightRef.current.get(cacheKey) === inflight) {
          debugHoverInflightRef.current.delete(cacheKey);
        }
      });
    }
    return inflight;
  }, []);

  const onDebugEditorMount: OnMount = useCallback((editor, monaco) => {
    debugEditorRef.current = editor;
    debugMonacoRef.current = monaco;
    setDebugEditorReadyTick((value) => value + 1);
    editor.updateOptions({
      readOnly: true,
      glyphMargin: true,
      hover: {
        enabled: true,
        delay: 220,
        sticky: true
      }
    });

    editor.onMouseDown((event) => {
      const session = selectedDebugSessionRef.current;
      const sessionReady = session?.phase === "attached" || session?.phase === "replay_ready";
      if (!session || !sessionReady || debugCommandLoadingRef.current) {
        return;
      }
      if (
        event.target.type !== monaco.editor.MouseTargetType.GUTTER_GLYPH_MARGIN &&
        event.target.type !== monaco.editor.MouseTargetType.GUTTER_LINE_NUMBERS
      ) {
        return;
      }
      const line = event.target.position?.lineNumber;
      if (!line) {
        return;
      }
      const isBreakpoint = session.breakpoints.includes(line);
      const request: DebugCommandRequest = isBreakpoint ? { kind: "clear_line", line } : { kind: "break_line", line };
      runDebugCommandRef.current?.(request).catch(() => {
        // handled by callback
      });
    });

    debugMouseMoveDisposableRef.current?.dispose();
    debugMouseMoveDisposableRef.current = editor.onMouseMove((event) => {
      const session = selectedDebugSessionRef.current;
      const sessionReady = session?.phase === "attached" || session?.phase === "replay_ready";
      if (!session || !sessionReady) {
        return;
      }
      const position = event.target.position;
      const model = editor.getModel();
      if (!position || !model) {
        return;
      }
      const word = model.getWordAtPosition(position);
      if (!word || !looksLikeIdentifier(word.word)) {
        debugHoverActiveKeyRef.current = "";
        setDebugHoveredVar("");
        setDebugHoverValue("");
        return;
      }

      const cacheKey = `${session.session_id}:${word.word}:${session.current_line ?? 0}`;
      if (debugHoverActiveKeyRef.current === cacheKey) {
        return;
      }
      debugHoverActiveKeyRef.current = cacheKey;
      setDebugHoveredVar(word.word);
      const cached = debugHoverCacheRef.current.get(cacheKey);
      if (cached) {
        setDebugHoverValue(cached);
        return;
      }
      setDebugHoverValue("(loading)");
      resolveDebugHoverValue(session, word.word)
        .then((value) => {
          if (debugHoverActiveKeyRef.current !== cacheKey) {
            return;
          }
          setDebugHoverValue(value ?? "(unavailable)");
        })
        .catch(() => {
          if (debugHoverActiveKeyRef.current !== cacheKey) {
            return;
          }
          setDebugHoverValue("(unavailable)");
        });
    });

    debugMouseLeaveDisposableRef.current?.dispose();
    debugMouseLeaveDisposableRef.current = editor.onMouseLeave(() => {
      debugHoverActiveKeyRef.current = "";
      setDebugHoveredVar("");
      setDebugHoverValue("");
    });
  }, [resolveDebugHoverValue]);

  useEffect(() => {
    const editor = debugEditorRef.current;
    const monaco = debugMonacoRef.current;
    if (!editor || !monaco) {
      return;
    }
    const model = editor.getModel();
    if (!model) {
      return;
    }

    debugHoverProviderDisposableRef.current?.dispose();
    debugHoverProviderDisposableRef.current = monaco.languages.registerHoverProvider(model.getLanguageId(), {
      provideHover: async (hoverModel, position) => {
        const session = selectedDebugSessionRef.current;
        const sessionReady = session?.phase === "attached" || session?.phase === "replay_ready";
        if (!session || !sessionReady) {
          return null;
        }
        if (hoverModel.uri.toString() !== model.uri.toString()) {
          return null;
        }
        const word = hoverModel.getWordAtPosition(position);
        if (!word || !looksLikeIdentifier(word.word)) {
          return null;
        }

        const cacheKey = `${session.session_id}:${word.word}:${session.current_line ?? 0}`;
        const value = await resolveDebugHoverValue(session, word.word);
        if (!value) {
          return null;
        }
        setDebugHoveredVar(word.word);
        setDebugHoverValue(value);

        return {
          range: new monaco.Range(position.lineNumber, word.startColumn, position.lineNumber, word.endColumn),
          contents: [{ value: `**${word.word}**` }, { value: `\`\`\`text\n${value}\n\`\`\`` }]
        };
      }
    });
    return () => {
      debugHoverProviderDisposableRef.current?.dispose();
      debugHoverProviderDisposableRef.current = null;
    };
  }, [debugEditorReadyTick, resolveDebugHoverValue, selectedDebugSessionId, selectedDebugSession?.source_code, selectedDebugSession?.source_flavor]);

  useEffect(() => {
    const editor = debugEditorRef.current;
    const monaco = debugMonacoRef.current;
    if (!editor || !monaco || !selectedDebugSession?.source_code) {
      if (editor) {
        debugDecorationIdsRef.current = editor.deltaDecorations(debugDecorationIdsRef.current, []);
      }
      return;
    }

    const decorations: Monaco.editor.IModelDeltaDecoration[] = [];
    const currentLine = selectedDebugSession.current_line;
    if (currentLine && currentLine > 0) {
      decorations.push({
        range: new monaco.Range(currentLine, 1, currentLine, 1),
        options: {
          isWholeLine: true,
          className: "pd-debug-current-line",
          linesDecorationsClassName: "pd-debug-current-line-marker"
        }
      });
    }
    for (const line of selectedDebugSession.breakpoints) {
      decorations.push({
        range: new monaco.Range(line, 1, line, 1),
        options: {
          isWholeLine: true,
          glyphMarginClassName: "pd-debug-breakpoint-glyph",
          glyphMarginHoverMessage: { value: "Breakpoint" }
        }
      });
    }
    debugDecorationIdsRef.current = editor.deltaDecorations(debugDecorationIdsRef.current, decorations);
  }, [debugEditorReadyTick, selectedDebugSession?.source_code, selectedDebugSession?.current_line, selectedDebugSession?.breakpoints]);

  useEffect(() => {
    debugHoverCacheRef.current.clear();
    debugHoverInflightRef.current.clear();
    debugHoverActiveKeyRef.current = "";
  }, [selectedDebugSessionId, selectedDebugSession?.current_line]);

  useEffect(() => {
    if (!selectedDebugSessionId) {
      return;
    }
    const timer = window.setInterval(() => {
      loadDebugSessionDetail(selectedDebugSessionId).catch(() => {
        // ignore background refresh errors
      });
      loadDebugSessions().catch(() => {
        // ignore background refresh errors
      });
    }, 1000);
    return () => window.clearInterval(timer);
  }, [loadDebugSessionDetail, loadDebugSessions, selectedDebugSessionId]);

  useEffect(() => {
    setDebugHoveredVar("");
    setDebugHoverValue("");
  }, [selectedDebugSessionId, selectedDebugSession?.current_line]);

  useEffect(() => {
    return () => {
      debugHoverProviderDisposableRef.current?.dispose();
      debugHoverProviderDisposableRef.current = null;
      debugMouseMoveDisposableRef.current?.dispose();
      debugMouseMoveDisposableRef.current = null;
      debugMouseLeaveDisposableRef.current?.dispose();
      debugMouseLeaveDisposableRef.current = null;
    };
  }, []);

  const debugSessionsSorted = useMemo(() => {
    return [...debugSessions].sort((lhs, rhs) => rhs.updated_unix_ms - lhs.updated_unix_ms);
  }, [debugSessions]);

  const debugStartDisabledReason = useMemo(() => {
    if (!debugEdgeId) {
      return "Select an edge first.";
    }
    const selectedEdgeSummary = edgeSummaries.find((edge) => edge.edge_id === debugEdgeId);
    if (!selectedEdgeSummary) {
      return "Selected edge is not available.";
    }
    if (!selectedEdgeSummary.last_telemetry) {
      return "No telemetry yet for this edge. Wait for it to poll the controller.";
    }
    if (!selectedEdgeSummary.last_telemetry.program_loaded) {
      return "This edge has no loaded program yet. Apply a program before starting debug.";
    }
    if (debugMode === "recording" && !debugRequestPath.trim()) {
      return "Recording mode requires a request path.";
    }
    if (debugMode === "recording") {
      const parsed = Number.parseInt(debugRecordCount, 10);
      if (!Number.isFinite(parsed) || parsed < 1) {
        return "Record count must be >= 1.";
      }
    }
    return null;
  }, [debugEdgeId, debugMode, debugRecordCount, debugRequestPath, edgeSummaries]);

  return {
    debugView,
    setDebugView,
    debugEdgeId,
    setDebugEdgeId,
    debugMode,
    setDebugMode,
    debugHeaderName,
    setDebugHeaderName,
    debugRequestPath,
    setDebugRequestPath,
    debugRecordCount,
    setDebugRecordCount,
    debugCreating,
    debugSessionsSorted,
    selectedDebugSessionId,
    selectDebugSession,
    selectedDebugSession,
    runDebugCommand,
    stopDebugSession,
    deleteDebugSession,
    debugCommandLoading,
    onDebugEditorMount,
    debugHoveredVar,
    debugHoverValue,
    debugStartDisabledReason,
    selectRecording,
    createDebugSession,
    loadDebugSessions
  };
}
