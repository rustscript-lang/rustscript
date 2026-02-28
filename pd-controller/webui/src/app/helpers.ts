import type {
  DebugSessionMode,
  DebugSessionPhase,
  EdgeSummary,
  EdgeTrafficPoint,
  FlowEdge,
  FlowNode,
  SourceFlavor,
  UiBlockDefinition,
  UiGraphEdge,
  UiGraphEdgeWire
} from "@/app/types";

export function normalizeFlowEdges(edges: FlowEdge[]): FlowEdge[] {
  const normalized: FlowEdge[] = [];
  for (const edge of edges) {
    const sourceHandle = edge.sourceHandle ?? edge.data?.source_output ?? null;
    const targetHandle = edge.targetHandle ?? edge.data?.target_input ?? null;
    if (!edge.source || !edge.target || !sourceHandle || !targetHandle) {
      continue;
    }
    normalized.push({
      ...edge,
      sourceHandle,
      targetHandle,
      data: {
        source_output: sourceHandle,
        target_input: targetHandle
      }
    });
  }
  return normalized;
}

export function defaultValues(definition: UiBlockDefinition): Record<string, string> {
  const values: Record<string, string> = {};
  for (const input of definition.inputs) {
    values[input.key] = input.default_value;
  }
  return values;
}

export function graphPayload(nodes: FlowNode[], edges: FlowEdge[]) {
  const mappedEdges = normalizeFlowEdges(edges)
    .map((edge) => {
      const sourceOutput = edge.sourceHandle;
      const targetInput = edge.targetHandle;
      if (!sourceOutput || !targetInput) {
        return null;
      }
      return {
        source: edge.source,
        source_output: sourceOutput,
        target: edge.target,
        target_input: targetInput
      };
    })
    .filter((edge): edge is UiGraphEdge => edge !== null);

  return {
    nodes: nodes.map((node) => ({
      id: node.id,
      block_id: node.data.blockId,
      values: node.data.values,
      position: {
        x: node.position.x,
        y: node.position.y
      }
    })),
    edges: mappedEdges
  };
}

export function applyConnectedInputs(nodes: FlowNode[], edges: FlowEdge[]): FlowNode[] {
  const connectionMap: Record<string, Record<string, boolean>> = {};
  for (const edge of normalizeFlowEdges(edges)) {
    const targetHandle = edge.targetHandle;
    if (!targetHandle) {
      continue;
    }
    if (!connectionMap[edge.target]) {
      connectionMap[edge.target] = {};
    }
    connectionMap[edge.target][targetHandle] = true;
  }
  return nodes.map((node) => ({
    ...node,
    data: {
      ...node.data,
      connectedInputs: connectionMap[node.id] ?? {}
    }
  }));
}

export function toFlowEdges(edges: UiGraphEdgeWire[], nodes: FlowNode[]): FlowEdge[] {
  const nodeMap = new Map(nodes.map((node) => [node.id, node]));
  const normalized: FlowEdge[] = [];
  for (const edge of edges) {
    const sourceHandle = edge.source_output ?? edge.sourceHandle ?? edge.data?.source_output ?? null;
    const targetHandle = edge.target_input ?? edge.targetHandle ?? edge.data?.target_input ?? null;
    if (!edge.source || !edge.target || !sourceHandle || !targetHandle) {
      continue;
    }
    const sourceNode = nodeMap.get(edge.source);
    const targetNode = nodeMap.get(edge.target);
    if (!sourceNode || !targetNode) {
      continue;
    }
    const sourceValid = sourceNode.data.definition.outputs.some((output) => output.key === sourceHandle);
    const targetValid =
      targetHandle === "__flow"
        ? targetNode.data.definition.accepts_flow
        : targetNode.data.definition.inputs.some((input) => input.key === targetHandle && input.connectable);
    if (!sourceValid || !targetValid) {
      continue;
    }
    normalized.push({
      id: `${edge.source}:${sourceHandle}->${edge.target}:${targetHandle}`,
      source: edge.source,
      sourceHandle,
      target: edge.target,
      targetHandle,
      data: { source_output: sourceHandle, target_input: targetHandle },
      type: "default",
      animated: true,
      style: { stroke: "#22d3ee", strokeWidth: 2 }
    });
  }
  return normalizeFlowEdges(normalized);
}

export function formatUnixMs(value: number | null | undefined): string {
  if (!value) {
    return "-";
  }
  return new Date(value).toLocaleString();
}

export function formatNumber(value: number): string {
  return Intl.NumberFormat().format(value);
}

export function edgeHealth(summary: EdgeSummary): "healthy" | "degraded" | "idle" {
  if (!summary.last_telemetry) {
    return "idle";
  }
  const telemetry = summary.last_telemetry;
  if (telemetry.control_rpc_polls_error_total > 0 || telemetry.control_rpc_results_error_total > 0) {
    return "degraded";
  }
  return "healthy";
}

export function edgeHealthClasses(summary: EdgeSummary): string {
  const health = edgeHealth(summary);
  if (health === "healthy") {
    return "text-emerald-600";
  }
  if (health === "degraded") {
    return "text-amber-600";
  }
  return "text-slate-500";
}

export function syncStatusClasses(status: EdgeSummary["sync_status"]): string {
  if (status === "synced") {
    return "text-emerald-600";
  }
  if (status === "out_of_sync") {
    return "text-amber-600";
  }
  return "text-slate-500";
}

export function debugPhaseClasses(phase: DebugSessionPhase, mode?: DebugSessionMode): string {
  if (phase === "stopped" && mode === "recording") {
    return "text-emerald-600";
  }
  if (phase === "attached" || phase === "replay_ready") {
    return "text-emerald-600";
  }
  if (
    phase === "waiting_for_attach" ||
    phase === "waiting_for_start_result" ||
    phase === "waiting_for_recordings" ||
    phase === "queued"
  ) {
    return "text-amber-600";
  }
  if (phase === "failed") {
    return "text-rose-600";
  }
  return "text-slate-600";
}

export function debugPhaseLabel(phase: DebugSessionPhase, mode?: DebugSessionMode): string {
  if (phase === "stopped" && mode === "recording") {
    return "completed";
  }
  return phase.split("_").join(" ");
}

export function looksLikeIdentifier(value: string): boolean {
  if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(value)) {
    return false;
  }
  const keywords = new Set([
    "if",
    "else",
    "for",
    "while",
    "loop",
    "let",
    "local",
    "import",
    "require",
    "return",
    "true",
    "false",
    "null"
  ]);
  return !keywords.has(value);
}

export function normalizeFlavor(value: string): SourceFlavor {
  const lower = value.trim().toLowerCase();
  if (lower === "javascript" || lower === "js") {
    return "javascript";
  }
  if (lower === "lua") {
    return "lua";
  }
  if (lower === "scheme" || lower === "scm") {
    return "scheme";
  }
  return "rustscript";
}

export function monacoLanguageForFlavor(flavor: SourceFlavor | string | null | undefined): string {
  const lower = (flavor ?? "").toLowerCase();
  if (lower === "javascript" || lower === "js") {
    return "javascript";
  }
  if (lower === "lua") {
    return "lua";
  }
  if (lower === "scheme" || lower === "scm") {
    return "scheme";
  }
  return "rust";
}

export type LineSeries = {
  key: string;
  stroke: string;
  valueFor: (point: EdgeTrafficPoint, index: number, points: EdgeTrafficPoint[]) => number;
};
