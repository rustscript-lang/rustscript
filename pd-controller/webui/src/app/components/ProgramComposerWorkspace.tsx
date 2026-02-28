import type { DragEvent } from "react";
import {
  Background,
  Controls,
  MiniMap,
  ReactFlow,
  type Connection,
  type EdgeChange,
  type NodeChange,
  type ReactFlowInstance,
  type Viewport
} from "@xyflow/react";

import { nodeTypes } from "@/app/components/BlockNode";
import { GeneratedCodePanel } from "@/app/components/GeneratedCodePanel";
import { ProgramPalette } from "@/app/components/ProgramPalette";
import type { FlowEdge, FlowNode, SourceFlavor, UiBlockDefinition, UiSourceBundle } from "@/app/types";
import { Card, CardContent, CardHeader } from "@/components/ui/card";
import { Button } from "@/components/ui/button";

type ProgramComposerWorkspaceProps = {
  isCodeEditMode: boolean;
  onExitCodeEditMode: () => void;
  showExitCodeEditButton?: boolean;
  onEnterCodeEditMode: () => void;
  source: UiSourceBundle;
  activeFlavor: SourceFlavor;
  rendering: boolean;
  onFlavorChange: (value: SourceFlavor) => void;
  onSourceChange: (flavor: SourceFlavor, value: string) => void;
  selectedProgramId: string | null;
  selectedVersion: number | null;
  graphCanvasRevision: number;
  nodes: FlowNode[];
  edges: FlowEdge[];
  onNodesChange: (changes: NodeChange<FlowNode>[]) => void;
  onEdgesChange: (changes: EdgeChange<FlowEdge>[]) => void;
  onConnect: (connection: Connection) => void;
  onInit: (instance: ReactFlowInstance<FlowNode, FlowEdge>) => void;
  onMoveEnd: (viewport: Viewport) => void;
  onCanvasDrop: (event: DragEvent<HTMLDivElement>) => void;
  selectedNodeCount: number;
  selectedEdgeCount: number;
  paletteMinimized: boolean;
  onTogglePaletteMinimized: () => void;
  codePanelMinimized: boolean;
  onToggleCodePanelMinimized: () => void;
  definitions: UiBlockDefinition[];
  search: string;
  onSearchChange: (value: string) => void;
  onPaletteDragStart: (event: DragEvent<HTMLDivElement>, blockId: string) => void;
  onAddNode: (blockId: string) => void;
};

export function ProgramComposerWorkspace({
  isCodeEditMode,
  onExitCodeEditMode,
  showExitCodeEditButton = true,
  onEnterCodeEditMode,
  source,
  activeFlavor,
  rendering,
  onFlavorChange,
  onSourceChange,
  selectedProgramId,
  selectedVersion,
  graphCanvasRevision,
  nodes,
  edges,
  onNodesChange,
  onEdgesChange,
  onConnect,
  onInit,
  onMoveEnd,
  onCanvasDrop,
  selectedNodeCount,
  selectedEdgeCount,
  paletteMinimized,
  onTogglePaletteMinimized,
  codePanelMinimized,
  onToggleCodePanelMinimized,
  definitions,
  search,
  onSearchChange,
  onPaletteDragStart,
  onAddNode
}: ProgramComposerWorkspaceProps) {
  if (isCodeEditMode) {
    return (
      <Card className="pd-panel-enter border-slate-200/80 bg-white/90 shadow-xl backdrop-blur">
        <CardHeader>
          <div className="flex items-center justify-between gap-3">
            <div>
              <div className="text-xs uppercase tracking-[0.24em] text-slate-500">Source Editor</div>
              <div className="mt-1 text-2xl font-semibold tracking-tight">Code</div>
              <div className="mt-1 text-sm text-muted-foreground">
                Code edit mode. This version is saved as source-only and does not keep flow graph state.
              </div>
            </div>
            {showExitCodeEditButton ? (
              <Button variant="outline" onClick={onExitCodeEditMode}>
                Exit Edit
              </Button>
            ) : null}
          </div>
        </CardHeader>
        <CardContent>
          <GeneratedCodePanel
            rendering={rendering}
            activeFlavor={activeFlavor}
            source={source}
            onFlavorChange={onFlavorChange}
            readOnly={false}
            enableLint
            editorHeight="calc(100dvh - 360px)"
            onCodeChange={onSourceChange}
            showHeader={false}
          />
        </CardContent>
      </Card>
    );
  }

  return (
    <>
      <div className="relative overflow-hidden rounded-xl border border-slate-800 bg-slate-950 text-slate-100 shadow-xl">
        <div
          className="h-[calc(100dvh-320px)] min-h-[420px] w-full"
          onDragOver={(event) => event.preventDefault()}
          onDrop={onCanvasDrop}
        >
          <ReactFlow<FlowNode, FlowEdge>
            nodes={nodes}
            edges={edges}
            nodeTypes={nodeTypes}
            onNodesChange={onNodesChange}
            onEdgesChange={onEdgesChange}
            onConnect={onConnect}
            onInit={onInit}
            onMoveEnd={(_, viewport) => onMoveEnd(viewport)}
            minZoom={0.2}
            maxZoom={2}
            defaultViewport={{ x: 0, y: 0, zoom: 1 }}
            defaultEdgeOptions={{
              type: "default",
              animated: true,
              style: { stroke: "#22d3ee", strokeWidth: 2 }
            }}
          >
            <Background color="#1e293b" gap={22} size={1} />
            <MiniMap
              position="bottom-left"
              className="!bg-slate-900"
              nodeColor="#334155"
              maskColor="rgba(15, 23, 42, 0.45)"
            />
            <Controls position="bottom-right" className="!bg-slate-900 !text-slate-200" />
          </ReactFlow>
        </div>

        <div className="pointer-events-none absolute bottom-12 left-4 top-4 z-20 hidden xl:block">
          <ProgramPalette
            floating
            minimized={paletteMinimized}
            onToggleMinimized={onTogglePaletteMinimized}
            definitions={definitions}
            search={search}
            onSearchChange={onSearchChange}
            onPaletteDragStart={onPaletteDragStart}
            onAddNode={onAddNode}
          />
        </div>

        <div className="pointer-events-none absolute bottom-12 right-4 top-4 z-20 hidden xl:block">
          <GeneratedCodePanel
            floating
            minimized={codePanelMinimized}
            onToggleMinimized={onToggleCodePanelMinimized}
            rendering={rendering}
            activeFlavor={activeFlavor}
            source={source}
            onFlavorChange={onFlavorChange}
            onEdit={onEnterCodeEditMode}
          />
        </div>

        <div className="absolute bottom-0 left-0 right-0 border-t border-slate-800 bg-slate-900/70 px-3 py-2 text-xs text-slate-300">
          nodes={nodes.length} edges={edges.length} selected_nodes={selectedNodeCount} selected_edges={selectedEdgeCount}
        </div>
      </div>

      <div className="grid grid-cols-1 gap-4 xl:hidden">
        <ProgramPalette
          definitions={definitions}
          search={search}
          onSearchChange={onSearchChange}
          onPaletteDragStart={onPaletteDragStart}
          onAddNode={onAddNode}
        />
        <GeneratedCodePanel
          rendering={rendering}
          activeFlavor={activeFlavor}
          source={source}
          onFlavorChange={onFlavorChange}
          onEdit={onEnterCodeEditMode}
        />
      </div>
    </>
  );
}
