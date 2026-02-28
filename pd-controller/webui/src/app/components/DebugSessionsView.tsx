import Editor, { type OnMount } from "@monaco-editor/react";
import {
  ArrowLeft,
  ArrowRight,
  ChevronsRight,
  CornerUpLeft,
  Crosshair,
  Layers,
  List,
  Play,
  Square
} from "lucide-react";

import { debugPhaseClasses, debugPhaseLabel, formatUnixMs, monacoLanguageForFlavor } from "@/app/helpers";
import { RowActionMenu } from "@/app/components/RowActionMenu";
import type {
  DebugSessionDetail,
  DebugSessionSummary,
  EdgeSummary,
  RunDebugCommandFn
} from "@/app/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";

type DebugSessionsViewProps = {
  debugView: "list" | "detail";
  onBackToList: () => void;
  debugEdgeId: string;
  onDebugEdgeIdChange: (value: string) => void;
  edgeSummaries: EdgeSummary[];
  debugMode: "interactive" | "recording";
  onDebugModeChange: (value: "interactive" | "recording") => void;
  debugHeaderName: string;
  onDebugHeaderNameChange: (value: string) => void;
  debugRequestPath: string;
  onDebugRequestPathChange: (value: string) => void;
  debugRecordCount: string;
  onDebugRecordCountChange: (value: string) => void;
  onCreateDebugSession: () => void;
  debugCreating: boolean;
  startDisabledReason: string | null;
  debugSessionsSorted: DebugSessionSummary[];
  selectedDebugSessionId: string | null;
  onSelectDebugSession: (sessionId: string) => Promise<void>;
  selectedDebugSession: DebugSessionDetail | null;
  runDebugCommand: RunDebugCommandFn;
  onStopDebugSession: (sessionId?: string) => void;
  onDeleteDebugSession: (sessionId?: string) => void;
  debugCommandLoading: boolean;
  onDebugEditorMount: OnMount;
  debugHoveredVar: string;
  debugHoverValue: string;
  onSelectRecording: (recordingId: string) => Promise<void>;
};

export function DebugSessionsView({
  debugView,
  onBackToList,
  debugEdgeId,
  onDebugEdgeIdChange,
  edgeSummaries,
  debugMode,
  onDebugModeChange,
  debugHeaderName,
  onDebugHeaderNameChange,
  debugRequestPath,
  onDebugRequestPathChange,
  debugRecordCount,
  onDebugRecordCountChange,
  onCreateDebugSession,
  debugCreating,
  startDisabledReason,
  debugSessionsSorted,
  selectedDebugSessionId,
  onSelectDebugSession,
  selectedDebugSession,
  runDebugCommand,
  onStopDebugSession,
  onDeleteDebugSession,
  debugCommandLoading,
  onDebugEditorMount,
  debugHoveredVar,
  debugHoverValue,
  onSelectRecording
}: DebugSessionsViewProps) {
  let activeCount = 0;
  let waitingCount = 0;
  let stoppedCount = 0;
  let failedCount = 0;
  for (const session of debugSessionsSorted) {
    if (session.phase === "attached" || session.phase === "replay_ready") {
      activeCount += 1;
    } else if (
      session.phase === "queued" ||
      session.phase === "waiting_for_start_result" ||
      session.phase === "waiting_for_recordings" ||
      session.phase === "waiting_for_attach"
    ) {
      waitingCount += 1;
    } else if (session.phase === "stopped") {
      stoppedCount += 1;
    } else if (session.phase === "failed") {
      failedCount += 1;
    }
  }

  if (debugView === "list") {
    return (
      <div className="space-y-4">
        <Card className="border-slate-200/80 bg-white/80 backdrop-blur">
          <CardHeader>
            <div className="text-xs uppercase tracking-[0.24em] text-slate-500">Runtime Diagnostics</div>
            <div className="mt-1 text-2xl font-semibold tracking-tight">Debug Sessions</div>
            <div className="mt-1 text-sm text-muted-foreground">
              Start a remote debug session and track its lifecycle across edges.
            </div>
          </CardHeader>
        </Card>

        <Card className="border-slate-200/80 bg-white/80 backdrop-blur">
          <CardHeader className="pb-3">
            <div className="text-xs uppercase tracking-[0.24em] text-slate-500">Session Health</div>
            <div className="mt-1 text-2xl font-semibold tracking-tight">Overview</div>
            <div className="mt-1 text-sm text-muted-foreground">Active, waiting, stopped, and failed sessions.</div>
          </CardHeader>
          <CardContent className="grid grid-cols-2 gap-3 sm:grid-cols-5">
            <div className="rounded-md border bg-background/70 p-3">
              <div className="text-xs uppercase tracking-wide text-muted-foreground">Total</div>
              <div className="text-xl font-semibold">{debugSessionsSorted.length}</div>
            </div>
            <div className="rounded-md border bg-background/70 p-3">
              <div className="text-xs uppercase tracking-wide text-muted-foreground">Active</div>
              <div className="text-xl font-semibold text-emerald-600">{activeCount}</div>
            </div>
            <div className="rounded-md border bg-background/70 p-3">
              <div className="text-xs uppercase tracking-wide text-muted-foreground">Waiting</div>
              <div className="text-xl font-semibold text-amber-600">{waitingCount}</div>
            </div>
            <div className="rounded-md border bg-background/70 p-3">
              <div className="text-xs uppercase tracking-wide text-muted-foreground">Stopped</div>
              <div className="text-xl font-semibold">{stoppedCount}</div>
            </div>
            <div className="rounded-md border bg-background/70 p-3">
              <div className="text-xs uppercase tracking-wide text-muted-foreground">Failed</div>
              <div className="text-xl font-semibold text-rose-600">{failedCount}</div>
            </div>
          </CardContent>
        </Card>

        <Card className="border-slate-200/80 bg-white/80 backdrop-blur">
          <CardHeader className="pb-3">
            <div className="text-xs uppercase tracking-[0.24em] text-slate-500">Operational View</div>
            <div className="mt-1 text-2xl font-semibold tracking-tight">Session List</div>
            <div className="mt-1 text-sm text-muted-foreground">Click a row to open debug session detail.</div>
          </CardHeader>
          <CardContent>
            <div className="mb-3 grid grid-cols-1 gap-3 md:grid-cols-[minmax(180px,1fr)_minmax(220px,1.2fr)_auto] md:items-end">
              <div className="space-y-1">
                <Label htmlFor="debug-edge">Edge</Label>
                <select
                  id="debug-edge"
                  value={debugEdgeId}
                  onChange={(event) => onDebugEdgeIdChange(event.target.value)}
                  className="h-9 w-full rounded-md border bg-background px-2 text-sm"
                >
                  <option value="">Select edge</option>
                  {edgeSummaries.map((edge) => (
                    <option key={edge.edge_id} value={edge.edge_id}>
                      {edge.edge_name}
                    </option>
                  ))}
                </select>
              </div>
              <div className="space-y-1">
                <Label htmlFor="debug-mode">Mode</Label>
                <select
                  id="debug-mode"
                  value={debugMode}
                  onChange={(event) => onDebugModeChange(event.target.value as "interactive" | "recording")}
                  className="h-9 w-full rounded-md border bg-background px-2 text-sm"
                >
                  <option value="interactive">Interactive</option>
                  <option value="recording">Recording</option>
                </select>
              </div>
              <Button onClick={onCreateDebugSession} disabled={debugCreating || !!startDisabledReason}>
                {debugCreating ? "Creating..." : "Start Session"}
              </Button>
            </div>
            {debugMode === "interactive" ? (
              <div className="mb-3 grid grid-cols-1 gap-3">
                <div className="space-y-1">
                  <Label htmlFor="debug-header-name">Header Name</Label>
                  <Input
                    id="debug-header-name"
                    value={debugHeaderName}
                    onChange={(event) => onDebugHeaderNameChange(event.target.value)}
                    placeholder="x-pd-debug-nonce"
                  />
                </div>
              </div>
            ) : (
              <div className="mb-3 grid grid-cols-1 gap-3 md:grid-cols-2">
                <div className="space-y-1">
                  <Label htmlFor="debug-request-path">Request Path</Label>
                  <Input
                    id="debug-request-path"
                    value={debugRequestPath}
                    onChange={(event) => onDebugRequestPathChange(event.target.value)}
                    placeholder="/api/example"
                  />
                </div>
                <div className="space-y-1">
                  <Label htmlFor="debug-record-count">Record Count</Label>
                  <Input
                    id="debug-record-count"
                    type="number"
                    min={1}
                    value={debugRecordCount}
                    onChange={(event) => onDebugRecordCountChange(event.target.value)}
                  />
                </div>
              </div>
            )}
            {startDisabledReason ? <div className="mb-3 text-xs text-muted-foreground">{startDisabledReason}</div> : null}
            <div className="overflow-hidden rounded-lg border">
              <div className="grid grid-cols-[minmax(220px,1.4fr)_120px_160px_200px_48px] gap-2 border-b bg-muted/40 px-3 py-2 text-[11px] uppercase tracking-wide text-muted-foreground">
                <div>Session</div>
                <div>Mode</div>
                <div>Status</div>
                <div>Last Updated</div>
                <div className="text-right">Actions</div>
              </div>
              <div className="max-h-[66vh] overflow-auto">
                {debugSessionsSorted.map((session) => (
                  <div
                    key={session.session_id}
                    className={`grid w-full grid-cols-[minmax(220px,1.4fr)_120px_160px_200px_48px] items-center gap-2 border-b px-3 py-2 text-left text-sm transition hover:bg-muted/50 ${
                      selectedDebugSessionId === session.session_id ? "bg-primary/5" : ""
                    }`}
                  >
                    <button
                      type="button"
                      onClick={() => {
                        onSelectDebugSession(session.session_id).catch(() => {
                          // handled by callback
                        });
                      }}
                      className="col-span-4 grid min-w-0 grid-cols-[minmax(220px,1.4fr)_120px_160px_200px] items-center gap-2 text-left"
                    >
                      <div className="min-w-0">
                        <div className="truncate font-medium">{session.edge_name}</div>
                        <div className="mt-1 truncate text-xs text-muted-foreground">session={session.session_id}</div>
                        {session.request_id ? (
                          <div className="truncate text-[11px] text-muted-foreground">request={session.request_id}</div>
                        ) : null}
                      </div>
                      <div className="text-xs uppercase text-muted-foreground">{session.mode}</div>
                      <div>
                        <Badge className={`rounded-full px-2 py-0 text-[10px] font-semibold uppercase ${debugPhaseClasses(session.phase, session.mode)}`}>
                          {debugPhaseLabel(session.phase, session.mode)}
                        </Badge>
                      </div>
                      <div className="text-xs text-muted-foreground">{formatUnixMs(session.updated_unix_ms)}</div>
                    </button>
                    <RowActionMenu
                      disabled={debugCommandLoading}
                      onDelete={() => onDeleteDebugSession(session.session_id)}
                    />
                  </div>
                ))}
                {debugSessionsSorted.length === 0 ? (
                  <div className="px-3 py-6 text-center text-sm text-muted-foreground">No debug sessions yet.</div>
                ) : null}
              </div>
            </div>
          </CardContent>
        </Card>
      </div>
    );
  }

  return (
    <div className="space-y-4">
      <Card className="border-slate-200/80 bg-white/80 backdrop-blur">
        <CardHeader>
          <div className="flex items-center justify-between gap-3">
            <div>
              <div className="text-xs uppercase tracking-[0.24em] text-slate-500">Inspector</div>
              <div className="mt-1 text-2xl font-semibold tracking-tight">Session Detail</div>
              <div className="mt-1 text-sm text-muted-foreground">
                {selectedDebugSession ? selectedDebugSession.edge_name : "Select a session from the list"}
              </div>
            </div>
            <Button variant="outline" onClick={onBackToList} className="inline-flex items-center gap-1">
              <ArrowLeft className="h-4 w-4" />
              Back To Sessions
            </Button>
          </div>
        </CardHeader>
        <CardContent>
          {selectedDebugSession ? (
            <div className="space-y-4">
              <div className="grid grid-cols-1 gap-2 lg:grid-cols-[1fr_auto] lg:items-start">
                <div className="space-y-2">
                  <div className="rounded-md border bg-background/70 p-2 text-xs">
                    <div className="flex items-center gap-2">
                      <span className="uppercase tracking-wide text-muted-foreground">Phase</span>
                      <Badge className={`rounded-full px-2 py-0 text-[10px] font-semibold uppercase ${debugPhaseClasses(selectedDebugSession.phase, selectedDebugSession.mode)}`}>
                        {debugPhaseLabel(selectedDebugSession.phase, selectedDebugSession.mode)}
                      </Badge>
                    </div>
                    <div className="mt-1 font-mono">session_id={selectedDebugSession.session_id}</div>
                    <div className="font-mono">edge_id={selectedDebugSession.edge_id}</div>
                    <div className="font-mono">mode={selectedDebugSession.mode}</div>
                    <div className="font-mono">
                      request_id={selectedDebugSession.request_id ?? "(none)"}
                    </div>
                    {selectedDebugSession.header_name && selectedDebugSession.nonce_header_value ? (
                      <div className="mt-2 rounded-md border bg-amber-50 p-2 text-[11px] text-amber-800">
                        trigger header:{" "}
                        <span className="font-mono">
                          {selectedDebugSession.header_name}: {selectedDebugSession.nonce_header_value}
                        </span>
                      </div>
                    ) : null}
                    {selectedDebugSession.message ? (
                      <div className="mt-2 text-muted-foreground">{selectedDebugSession.message}</div>
                    ) : null}
                    {selectedDebugSession.mode === "recording" ? (
                      <div className="mt-2 rounded-md border bg-slate-50 p-2 text-[11px] text-slate-700">
                        request_path: <span className="font-mono">{selectedDebugSession.request_path ?? "(any)"}</span>
                        <div>
                          recordings: {selectedDebugSession.recordings.length}
                          {selectedDebugSession.recording_target_count !== null
                            ? ` / ${selectedDebugSession.recording_target_count}`
                            : ""}
                        </div>
                        <div>
                          latest_request_id:{" "}
                          <span className="font-mono">{selectedDebugSession.request_id ?? "(none)"}</span>
                        </div>
                      </div>
                    ) : null}
                  </div>
                </div>
              </div>

              {selectedDebugSession.mode === "recording" ? (
                <div className="rounded-md border bg-background/70 p-2">
                  <div className="mb-1 text-[11px] uppercase tracking-wide text-muted-foreground">Recordings</div>
                  <div className="max-h-[180px] overflow-auto rounded border">
                    {selectedDebugSession.recordings.map((recording) => (
                      <button
                        key={recording.recording_id}
                        type="button"
                        onClick={() => {
                          onSelectRecording(recording.recording_id).catch(() => {
                            // handled by callback
                          });
                        }}
                        className={`grid w-full grid-cols-[70px_minmax(80px,1fr)_minmax(160px,1.2fr)_120px] gap-2 border-b px-2 py-1.5 text-left text-xs hover:bg-muted/40 ${
                          selectedDebugSession.selected_recording_id === recording.recording_id ? "bg-primary/5" : ""
                        }`}
                      >
                        <div>#{recording.sequence}</div>
                        <div className="truncate">{recording.frame_count} frames</div>
                        <div className="truncate font-mono">
                          {recording.request_id ? `request=${recording.request_id}` : "request=(none)"}
                        </div>
                        <div className="truncate">{formatUnixMs(recording.created_unix_ms)}</div>
                      </button>
                    ))}
                    {selectedDebugSession.recordings.length === 0 ? (
                      <div className="px-2 py-3 text-xs text-muted-foreground">No recordings captured yet.</div>
                    ) : null}
                  </div>
                </div>
              ) : null}

              <div className="rounded-md border bg-background/70 p-1.5">
                <div className="flex flex-wrap items-center gap-1">
                  {(() => {
                    const canControl =
                      selectedDebugSession.phase === "attached" || selectedDebugSession.phase === "replay_ready";
                    return (
                      <>
                  <Button
                    size="sm"
                    variant="outline"
                    className="h-8 w-8 p-0"
                    title="Where"
                    aria-label="Where"
                    onClick={() => runDebugCommand({ kind: "where" })}
                    disabled={debugCommandLoading || !canControl}
                  >
                    <Crosshair className="h-4 w-4" />
                  </Button>
                  <Button
                    size="sm"
                    variant="outline"
                    className="h-8 w-8 p-0"
                    title="Locals"
                    aria-label="Locals"
                    onClick={() => runDebugCommand({ kind: "locals" })}
                    disabled={debugCommandLoading || !canControl}
                  >
                    <List className="h-4 w-4" />
                  </Button>
                  <Button
                    size="sm"
                    variant="outline"
                    className="h-8 w-8 p-0"
                    title="Stack"
                    aria-label="Stack"
                    onClick={() => runDebugCommand({ kind: "stack" })}
                    disabled={debugCommandLoading || !canControl}
                  >
                    <Layers className="h-4 w-4" />
                  </Button>
                  <div className="mx-1 h-5 w-px bg-border" />
                  <Button
                    size="sm"
                    className="h-8 w-8 p-0"
                    title="Step"
                    aria-label="Step"
                    onClick={() => runDebugCommand({ kind: "step" })}
                    disabled={debugCommandLoading || !canControl}
                  >
                    <ArrowRight className="h-4 w-4" />
                  </Button>
                  <Button
                    size="sm"
                    className="h-8 w-8 p-0"
                    title="Next"
                    aria-label="Next"
                    onClick={() => runDebugCommand({ kind: "next" })}
                    disabled={debugCommandLoading || !canControl}
                  >
                    <ChevronsRight className="h-4 w-4" />
                  </Button>
                  <Button
                    size="sm"
                    className="h-8 w-8 p-0"
                    title="Out"
                    aria-label="Out"
                    onClick={() => runDebugCommand({ kind: "out" })}
                    disabled={debugCommandLoading || !canControl}
                  >
                    <CornerUpLeft className="h-4 w-4" />
                  </Button>
                  <Button
                    size="sm"
                    className="h-8 w-8 p-0"
                    title="Continue"
                    aria-label="Continue"
                    onClick={() => runDebugCommand({ kind: "continue" })}
                    disabled={debugCommandLoading || !canControl}
                  >
                    <Play className="h-4 w-4" />
                  </Button>
                      </>
                    );
                  })()}
                  <div className="mx-1 h-5 w-px bg-border" />
                  <Button
                    size="sm"
                    variant="outline"
                    className="h-8 w-8 border-rose-300 p-0 text-rose-700 hover:bg-rose-50"
                    title="Stop"
                    aria-label="Stop"
                    onClick={() => onStopDebugSession(selectedDebugSession.session_id)}
                    disabled={
                      debugCommandLoading ||
                      selectedDebugSession.phase === "stopped" ||
                      selectedDebugSession.phase === "failed"
                    }
                  >
                    <Square className="h-4 w-4" />
                  </Button>
                </div>
              </div>

              {selectedDebugSession.phase === "waiting_for_attach" || selectedDebugSession.phase === "waiting_for_start_result" ? (
                <div className="rounded-md border bg-background/70 p-3 text-sm text-muted-foreground">
                  Waiting for attach. Send a request to the selected edge with the trigger header shown above.
                </div>
              ) : null}

              {selectedDebugSession.phase === "waiting_for_recordings" ? (
                <div className="rounded-md border bg-background/70 p-3 text-sm text-muted-foreground">
                  Waiting for recordings. Send matching requests to capture traces.
                </div>
              ) : null}

              {selectedDebugSession.last_output ? (
                <div className="rounded-md border bg-background/70 p-2">
                  <div className="mb-1 text-[11px] uppercase tracking-wide text-muted-foreground">Last Debugger Output</div>
                  <pre className="max-h-[180px] overflow-auto whitespace-pre-wrap text-xs">{selectedDebugSession.last_output}</pre>
                </div>
              ) : null}

              {selectedDebugSession.source_code ? (
                <div className="rounded-md border bg-slate-950 text-slate-100">
                  <div className="h-[68vh]">
                    <Editor
                      onMount={onDebugEditorMount}
                      language={monacoLanguageForFlavor(selectedDebugSession.source_flavor)}
                      value={selectedDebugSession.source_code}
                      theme="vs"
                      options={{
                        readOnly: true,
                        glyphMargin: true,
                        minimap: { enabled: false },
                        scrollBeyondLastLine: false,
                        automaticLayout: true,
                        wordWrap: "on",
                        fontSize: 13,
                        lineDecorationsWidth: 20,
                        lineNumbersMinChars: 3,
                        renderLineHighlight: "none"
                      }}
                    />
                  </div>
                </div>
              ) : (
                <div className="rounded-md border bg-background/70 p-3 text-sm text-muted-foreground">
                  No source available for this session. Apply a stored program version first.
                </div>
              )}

              {debugHoveredVar ? (
                <div className="rounded-md border bg-background/70 p-2 text-xs">
                  <span className="font-semibold">hover inspect:</span> {debugHoveredVar} ={" "}
                  <span className="font-mono">{debugHoverValue || "(loading)"}</span>
                </div>
              ) : null}
            </div>
          ) : (
            <div className="rounded-md border bg-background/70 p-4 text-sm text-muted-foreground">
              Select a debug session from the list.
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
