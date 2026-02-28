import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import "@xyflow/react/dist/style.css";

import { DebugSessionsView } from "@/app/components/DebugSessionsView";
import { EdgeDetailView } from "@/app/components/EdgeDetailView";
import { EdgeListView } from "@/app/components/EdgeListView";
import { NavBar } from "@/app/components/NavBar";
import { ProgramDetailView } from "@/app/components/ProgramDetailView";
import { ProgramListView } from "@/app/components/ProgramListView";
import { ConfirmDialog } from "@/app/components/ConfirmDialog";
import { useComposer } from "@/app/hooks/useComposer";
import { useDebugSessions } from "@/app/hooks/useDebugSessions";
import { useEdges } from "@/app/hooks/useEdges";
import { usePrograms } from "@/app/hooks/usePrograms";
import type { Section } from "@/app/types";
import { Card, CardContent } from "@/components/ui/card";

type RouteState =
  | { section: "edges"; edgeId?: string }
  | { section: "programs"; programId?: string; version?: number }
  | { section: "debug_sessions"; sessionId?: string };

type ConfirmState = {
  title: string;
  description: string;
  confirmLabel: string;
  action: () => Promise<void>;
};

function parseRouteFromLocation(): RouteState {
  const [rawHashPath, rawHashQuery = ""] = (window.location.hash.replace(/^#/, "") || "/edges").split("?");
  const normalizedPath = rawHashPath.startsWith("/") ? rawHashPath : `/${rawHashPath}`;
  const segments = normalizedPath
    .split("/")
    .filter((segment) => segment.length > 0)
    .map((segment) => decodeURIComponent(segment));

  if (segments.length === 0) {
    return { section: "edges" };
  }

  if (segments[0] === "edges") {
    return segments[1] ? { section: "edges", edgeId: segments[1] } : { section: "edges" };
  }

  if (segments[0] === "programs") {
    if (!segments[1]) {
      return { section: "programs" };
    }
    const versionParam = new URLSearchParams(rawHashQuery).get("version");
    const parsedVersion = versionParam ? Number.parseInt(versionParam, 10) : Number.NaN;
    return {
      section: "programs",
      programId: segments[1],
      version: Number.isNaN(parsedVersion) ? undefined : parsedVersion
    };
  }

  if (segments[0] === "debug-sessions" || segments[0] === "debug_sessions") {
    return segments[1] ? { section: "debug_sessions", sessionId: segments[1] } : { section: "debug_sessions" };
  }

  return { section: "edges" };
}

export default function App() {
  const [section, setSection] = useState<Section>("edges");
  const [error, setError] = useState("");
  const [confirmState, setConfirmState] = useState<ConfirmState | null>(null);
  const [confirmBusy, setConfirmBusy] = useState(false);
  const applyingRouteRef = useRef(false);
  const routeSyncReadyRef = useRef(false);

  const composer = useComposer({ onError: setError });
  const edges = useEdges({ onError: setError });
  const programs = usePrograms({
    onError: setError,
    showProgramsSection: () => setSection("programs"),
    onProgramDeleted: edges.clearApplyProgramForDeletedProgram,
    composer
  });
  const debugSessions = useDebugSessions({
    onError: setError,
    edgeSummaries: edges.edgeSummaries,
    showDebugSessionsSection: () => setSection("debug_sessions")
  });

  const selectedApplyProgram = useMemo(
    () => programs.programs.find((program) => program.program_id === edges.applyProgramId) ?? null,
    [edges.applyProgramId, programs.programs]
  );

  const canExitCodeEditMode = useMemo(() => {
    if (!programs.selectedProgram || programs.selectedVersion === null) {
      return true;
    }
    if (programs.selectedVersion === 0) {
      return true;
    }
    const selectedVersionMeta = programs.selectedProgram.versions.find(
      (item) => item.version === programs.selectedVersion
    );
    return selectedVersionMeta ? selectedVersionMeta.flow_synced : true;
  }, [programs.selectedProgram, programs.selectedVersion]);

  const applyRoute = useCallback(
    async (route: RouteState) => {
      applyingRouteRef.current = true;
      try {
        if (route.section === "edges") {
          setSection("edges");
          if (route.edgeId) {
            await edges.selectEdge(route.edgeId);
          } else {
            edges.setEdgeView("list");
          }
          return;
        }

        if (route.section === "programs") {
          setSection("programs");
          if (route.programId) {
            await programs.selectProgram(route.programId);
            if (route.version !== undefined) {
              await programs.selectProgramVersion(String(route.version));
            }
          } else {
            programs.setProgramView("list");
          }
          return;
        }

        setSection("debug_sessions");
        if (route.sessionId) {
          await debugSessions.selectDebugSession(route.sessionId);
        } else {
          debugSessions.setDebugView("list");
        }
      } finally {
        applyingRouteRef.current = false;
      }
    },
    [
      debugSessions.selectDebugSession,
      debugSessions.setDebugView,
      edges.selectEdge,
      edges.setEdgeView,
      programs.selectProgram,
      programs.selectProgramVersion,
      programs.setProgramView
    ]
  );

  useEffect(() => {
    Promise.all([
      composer.loadBlocks(),
      programs.loadPrograms(),
      edges.loadEdges(),
      debugSessions.loadDebugSessions()
    ])
      .then(async () => {
        await applyRoute(parseRouteFromLocation());
      })
      .catch((err) => {
        setError(err instanceof Error ? err.message : "failed to initialize ui");
      })
      .finally(() => {
        routeSyncReadyRef.current = true;
      });
    // Initial bootstrap should happen once; route/view changes are handled by hash sync effects.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const handlePopState = () => {
      void applyRoute(parseRouteFromLocation());
    };
    window.addEventListener("popstate", handlePopState);
    return () => {
      window.removeEventListener("popstate", handlePopState);
    };
  }, [applyRoute]);

  const currentRoute = useMemo(() => {
    if (section === "edges") {
      if (edges.edgeView === "detail" && edges.selectedEdgeId) {
        return `#/edges/${encodeURIComponent(edges.selectedEdgeId)}`;
      }
      return "#/edges";
    }
    if (section === "programs") {
      if (programs.programView === "composer" && programs.selectedProgramId) {
        const base = `#/programs/${encodeURIComponent(programs.selectedProgramId)}`;
        const versionBelongsToSelectedProgram =
          programs.selectedProgram?.program_id === programs.selectedProgramId &&
          programs.selectedVersion !== null &&
          (programs.selectedVersion === 0 ||
            programs.selectedProgram.versions.some((item) => item.version === programs.selectedVersion));
        if (versionBelongsToSelectedProgram) {
          return `${base}?version=${programs.selectedVersion}`;
        }
        return base;
      }
      return "#/programs";
    }
    if (debugSessions.debugView === "detail" && debugSessions.selectedDebugSessionId) {
      return `#/debug-sessions/${encodeURIComponent(debugSessions.selectedDebugSessionId)}`;
    }
    return "#/debug-sessions";
  }, [
    debugSessions.debugView,
    debugSessions.selectedDebugSessionId,
    edges.edgeView,
    edges.selectedEdgeId,
    programs.programView,
    programs.selectedProgram,
    programs.selectedProgramId,
    programs.selectedVersion,
    section
  ]);

  useEffect(() => {
    if (!routeSyncReadyRef.current || applyingRouteRef.current) {
      return;
    }
    const locationRoute = window.location.hash || "#/edges";
    if (locationRoute === currentRoute) {
      return;
    }
    window.history.pushState(null, "", currentRoute);
  }, [currentRoute]);

  const requestConfirm = useCallback((state: ConfirmState) => {
    setConfirmState(state);
    setConfirmBusy(false);
  }, []);

  const onConfirmDelete = useCallback(async () => {
    if (!confirmState) {
      return;
    }
    setConfirmBusy(true);
    try {
      await confirmState.action();
      setConfirmState(null);
    } finally {
      setConfirmBusy(false);
    }
  }, [confirmState]);

  return (
    <div className="flex min-h-screen bg-background text-foreground">
      <NavBar
        section={section}
        onSelectEdges={() => {
          setSection("edges");
          edges.setEdgeView("list");
        }}
        onSelectPrograms={() => {
          setSection("programs");
          programs.setProgramView("list");
        }}
        onSelectDebugSessions={() => {
          setSection("debug_sessions");
          debugSessions.setDebugView("list");
        }}
      />

      <main className="min-w-0 flex-1 bg-gradient-to-br from-slate-50 via-white to-sky-50 p-4 lg:p-6">
        {error ? (
          <Card className="mb-4 border-red-300 bg-red-50">
            <CardContent className="p-3 text-sm text-red-700">{error}</CardContent>
          </Card>
        ) : null}

        {section === "edges" ? (
          edges.edgeView === "list" ? (
            <EdgeListView
              edgeStats={edges.edgeStats}
              edgeSearch={edges.edgeSearch}
              onEdgeSearchChange={edges.setEdgeSearch}
              filteredEdges={edges.filteredEdges}
              onSelectEdge={edges.selectEdge}
              onRefreshEdges={() => {
                edges.refreshEdges().catch(() => {
                  // handled by callback
                });
              }}
              refreshing={edges.edgesRefreshing}
            />
          ) : (
            <EdgeDetailView
              selectedEdge={edges.selectedEdge}
              onBack={() => edges.setEdgeView("list")}
              programs={programs.programs}
              applyProgramId={edges.applyProgramId}
              onApplyProgramChange={edges.onApplyProgramChange}
              applyVersion={edges.applyVersion}
              onApplyVersionChange={edges.setApplyVersion}
              selectedApplyProgram={selectedApplyProgram}
              applyLoading={edges.applyLoading}
              applyStatus={edges.applyStatus}
              onApplyProgram={() => {
                edges.applyProgramToEdge(programs.programs).catch(() => {
                  // handled by callback
                });
              }}
            />
          )
        ) : section === "programs" ? (
          programs.programView === "list" ? (
            <ProgramListView
              creatingProgram={programs.creatingProgram}
              onCreateProgram={programs.createProgram}
              programSearch={programs.programSearch}
              onProgramSearchChange={programs.setProgramSearch}
              filteredPrograms={programs.filteredPrograms}
              onSelectProgram={programs.selectProgram}
              deletingProgram={programs.deletingProgram}
              onDeleteProgram={(program) => {
                requestConfirm({
                  title: "Delete",
                  description: `Delete "${program.name}" and all versions? This cannot be undone.`,
                  confirmLabel: "Delete",
                  action: async () => {
                    await programs.deleteProgram(program.program_id);
                  }
                });
              }}
            />
          ) : (
            <ProgramDetailView
              selectedProgram={programs.selectedProgram}
              programNameDraft={programs.programNameDraft}
              onProgramNameDraftChange={programs.setProgramNameDraft}
              selectedVersion={programs.selectedVersion}
              onSelectVersion={programs.selectProgramVersion}
              renamingProgram={programs.renamingProgram}
              onRenameProgram={programs.renameProgram}
              deletingProgram={programs.deletingProgram}
              onDeleteProgram={() => {
                if (!programs.selectedProgram) {
                  return;
                }
                const programId = programs.selectedProgram.program_id;
                const programName = programs.selectedProgram.name;
                requestConfirm({
                  title: "Delete",
                  description: `Delete "${programName}" and all versions? This cannot be undone.`,
                  confirmLabel: "Delete",
                  action: async () => {
                    await programs.deleteProgram(programId);
                  }
                });
              }}
              savingVersion={programs.savingVersion}
              canSaveVersion={composer.isCodeEditMode || composer.nodes.length > 0}
              onSaveVersion={programs.saveProgramVersion}
              graphStatus={composer.graphStatus}
              onBackToPrograms={() => programs.setProgramView("list")}
              isCodeEditMode={composer.isCodeEditMode}
              canExitCodeEditMode={canExitCodeEditMode}
              onExitCodeEditMode={() => composer.setIsCodeEditMode(false)}
              onEnterCodeEditMode={() => composer.setIsCodeEditMode(true)}
              source={composer.source}
              activeFlavor={composer.activeFlavor}
              rendering={composer.rendering}
              onFlavorChange={composer.setActiveFlavor}
              onSourceChange={composer.updateSourceText}
              selectedProgramId={programs.selectedProgramId}
              graphCanvasRevision={composer.graphCanvasRevision}
              nodes={composer.nodes}
              edges={composer.edges}
              onNodesChange={composer.onNodesChange}
              onEdgesChange={composer.onEdgesChange}
              onConnect={composer.onConnect}
              onInit={composer.onFlowInit}
              onMoveEnd={composer.onFlowMoveEnd}
              onCanvasDrop={composer.onCanvasDrop}
              selectedNodeCount={composer.selectedNodeCount}
              selectedEdgeCount={composer.selectedEdgeCount}
              paletteMinimized={composer.paletteMinimized}
              onTogglePaletteMinimized={() => composer.setPaletteMinimized((value) => !value)}
              codePanelMinimized={composer.codePanelMinimized}
              onToggleCodePanelMinimized={() => composer.setCodePanelMinimized((value) => !value)}
              definitions={composer.filteredDefinitions}
              search={composer.search}
              onSearchChange={composer.setSearch}
              onPaletteDragStart={composer.onPaletteDragStart}
              onAddNode={composer.addNode}
            />
          )
        ) : (
          <DebugSessionsView
            debugView={debugSessions.debugView}
            onBackToList={() => debugSessions.setDebugView("list")}
            debugEdgeId={debugSessions.debugEdgeId}
            onDebugEdgeIdChange={debugSessions.setDebugEdgeId}
            edgeSummaries={edges.edgeSummaries}
            debugMode={debugSessions.debugMode}
            onDebugModeChange={debugSessions.setDebugMode}
            debugHeaderName={debugSessions.debugHeaderName}
            onDebugHeaderNameChange={debugSessions.setDebugHeaderName}
            debugRequestPath={debugSessions.debugRequestPath}
            onDebugRequestPathChange={debugSessions.setDebugRequestPath}
            debugRecordCount={debugSessions.debugRecordCount}
            onDebugRecordCountChange={debugSessions.setDebugRecordCount}
            onCreateDebugSession={debugSessions.createDebugSession}
            debugCreating={debugSessions.debugCreating}
            startDisabledReason={debugSessions.debugStartDisabledReason}
            debugSessionsSorted={debugSessions.debugSessionsSorted}
            selectedDebugSessionId={debugSessions.selectedDebugSessionId}
            onSelectDebugSession={debugSessions.selectDebugSession}
            selectedDebugSession={debugSessions.selectedDebugSession}
            runDebugCommand={debugSessions.runDebugCommand}
            onStopDebugSession={(sessionId) => {
              debugSessions.stopDebugSession(sessionId).catch(() => {
                // handled by callback
              });
            }}
            onDeleteDebugSession={(sessionId) => {
              const target =
                debugSessions.debugSessionsSorted.find((item) => item.session_id === sessionId) ??
                (debugSessions.selectedDebugSession
                  ? {
                      session_id: debugSessions.selectedDebugSession.session_id,
                      edge_name: debugSessions.selectedDebugSession.edge_name,
                      phase: debugSessions.selectedDebugSession.phase
                    }
                  : null);
              if (!target) {
                return;
              }
              requestConfirm({
                title: "Delete",
                description: `Delete "${target.session_id}" on edge "${target.edge_name}"? Active items will be stopped first.`,
                confirmLabel: "Delete",
                action: async () => {
                  await debugSessions.deleteDebugSession(target.session_id);
                }
              });
            }}
            debugCommandLoading={debugSessions.debugCommandLoading}
            onDebugEditorMount={debugSessions.onDebugEditorMount}
            debugHoveredVar={debugSessions.debugHoveredVar}
            debugHoverValue={debugSessions.debugHoverValue}
            onSelectRecording={debugSessions.selectRecording}
          />
        )}
      </main>
      <ConfirmDialog
        open={confirmState !== null}
        title={confirmState?.title ?? ""}
        description={confirmState?.description ?? ""}
        confirmLabel={confirmState?.confirmLabel ?? "Delete"}
        busy={confirmBusy}
        onCancel={() => {
          if (confirmBusy) {
            return;
          }
          setConfirmState(null);
        }}
        onConfirm={() => {
          void onConfirmDelete();
        }}
      />
    </div>
  );
}
