import { useCallback, useEffect, useMemo, useState } from "react";

import { edgeHealth } from "@/app/helpers";
import type {
  EdgeDetailResponse,
  EdgeListResponse,
  EdgeSummary,
  ProgramSummary,
  QueueResponse
} from "@/app/types";

type UseEdgesArgs = {
  onError: (message: string) => void;
};

export function useEdges({ onError }: UseEdgesArgs) {
  const [edgeSummaries, setEdgeSummaries] = useState<EdgeSummary[]>([]);
  const [edgeView, setEdgeView] = useState<"list" | "detail">("list");
  const [selectedEdgeId, setSelectedEdgeId] = useState<string | null>(null);
  const [selectedEdge, setSelectedEdge] = useState<EdgeDetailResponse | null>(null);
  const [edgeSearch, setEdgeSearch] = useState("");
  const [applyProgramId, setApplyProgramId] = useState<string>("");
  const [applyVersion, setApplyVersion] = useState<string>("latest");
  const [applyLoading, setApplyLoading] = useState(false);
  const [applyStatus, setApplyStatus] = useState("");
  const [edgesRefreshing, setEdgesRefreshing] = useState(false);

  const loadEdges = useCallback(async () => {
    const response = await fetch("/v1/edges");
    if (!response.ok) {
      throw new Error(`failed to load edges (${response.status})`);
    }
    const data = (await response.json()) as EdgeListResponse;
    setEdgeSummaries(data.edges);
  }, []);

  const refreshEdges = useCallback(async () => {
    setEdgesRefreshing(true);
    onError("");
    try {
      await loadEdges();
    } catch (err) {
      onError(err instanceof Error ? err.message : "failed to refresh edges");
    } finally {
      setEdgesRefreshing(false);
    }
  }, [loadEdges, onError]);

  const loadEdgeDetail = useCallback(async (edgeId: string) => {
    const response = await fetch(`/v1/edges/${edgeId}`);
    if (!response.ok) {
      throw new Error(`failed to load edge detail (${response.status})`);
    }
    const detail = (await response.json()) as EdgeDetailResponse;
    setSelectedEdge(detail);
  }, []);

  const selectEdge = useCallback(
    async (edgeId: string) => {
      setSelectedEdgeId(edgeId);
      setEdgeView("detail");
      onError("");
      try {
        await loadEdgeDetail(edgeId);
      } catch (err) {
        onError(err instanceof Error ? err.message : "failed to load edge");
      }
    },
    [loadEdgeDetail, onError]
  );

  useEffect(() => {
    if (!selectedEdgeId) {
      return;
    }
    loadEdgeDetail(selectedEdgeId).catch(() => {
      // ignore silent refresh errors
    });
  }, [loadEdgeDetail, selectedEdgeId]);

  const onApplyProgramChange = useCallback((programId: string) => {
    setApplyProgramId(programId);
    setApplyVersion("latest");
  }, []);

  const clearApplyProgramForDeletedProgram = useCallback((programId: string) => {
    setApplyProgramId((current) => (current === programId ? "" : current));
  }, []);

  const applyProgramToEdge = useCallback(
    async (programs: ProgramSummary[]) => {
      if (!selectedEdgeId || !applyProgramId) {
        onError("select edge and program");
        return;
      }
      const selectedProgramForApply = programs.find((program) => program.program_id === applyProgramId) ?? null;
      if (!selectedProgramForApply) {
        onError("selected program was not found");
        return;
      }
      if (selectedProgramForApply.latest_version === 0) {
        onError("selected program has no versions; save a version in Programs before applying");
        return;
      }
      setApplyLoading(true);
      setApplyStatus("");
      onError("");
      try {
        const body: { program_id: string; version?: number } = {
          program_id: applyProgramId
        };
        if (applyVersion !== "latest") {
          const parsed = Number.parseInt(applyVersion, 10);
          if (!Number.isNaN(parsed)) {
            body.version = parsed;
          }
        }

        const response = await fetch(`/v1/edges/${selectedEdgeId}/commands/apply-program-version`, {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify(body)
        });
        if (!response.ok) {
          throw new Error(await response.text());
        }
        const queued = (await response.json()) as QueueResponse;
        setApplyStatus(`queued ${queued.command_id}, pending=${queued.pending_commands}`);
        await loadEdges();
        await loadEdgeDetail(selectedEdgeId);
      } catch (err) {
        onError(err instanceof Error ? err.message : "failed to apply program");
      } finally {
        setApplyLoading(false);
      }
    },
    [applyProgramId, applyVersion, loadEdgeDetail, loadEdges, onError, selectedEdgeId]
  );

  const filteredEdges = useMemo(() => {
    const keyword = edgeSearch.trim().toLowerCase();
    if (!keyword) {
      return edgeSummaries;
    }
    return edgeSummaries.filter((edge) => edge.edge_name.toLowerCase().includes(keyword));
  }, [edgeSearch, edgeSummaries]);

  const edgeStats = useMemo(() => {
    let healthy = 0;
    let degraded = 0;
    let pending = 0;
    for (const edge of edgeSummaries) {
      const health = edgeHealth(edge);
      if (health === "healthy") {
        healthy += 1;
      } else if (health === "degraded") {
        degraded += 1;
      }
      pending += edge.pending_commands;
    }
    return {
      total: edgeSummaries.length,
      healthy,
      degraded,
      pending
    };
  }, [edgeSummaries]);

  return {
    edgeSummaries,
    edgeView,
    setEdgeView,
    selectedEdgeId,
    selectedEdge,
    edgeSearch,
    setEdgeSearch,
    applyProgramId,
    applyVersion,
    setApplyVersion,
    applyLoading,
    applyStatus,
    filteredEdges,
    edgeStats,
    loadEdges,
    refreshEdges,
    edgesRefreshing,
    selectEdge,
    onApplyProgramChange,
    applyProgramToEdge,
    clearApplyProgramForDeletedProgram
  };
}
