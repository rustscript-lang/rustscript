import { Activity, ChevronRight, Circle, RefreshCw } from "lucide-react";

import { edgeHealth, edgeHealthClasses, formatNumber, formatUnixMs, syncStatusClasses } from "@/app/helpers";
import type { EdgeSummary } from "@/app/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader } from "@/components/ui/card";
import { Input } from "@/components/ui/input";

type EdgeStats = {
  total: number;
  healthy: number;
  degraded: number;
  pending: number;
};

type EdgeListViewProps = {
  edgeStats: EdgeStats;
  edgeSearch: string;
  onEdgeSearchChange: (value: string) => void;
  filteredEdges: EdgeSummary[];
  onSelectEdge: (edgeId: string) => void;
  onRefreshEdges: () => void;
  refreshing: boolean;
};

export function EdgeListView({
  edgeStats,
  edgeSearch,
  onEdgeSearchChange,
  filteredEdges,
  onSelectEdge,
  onRefreshEdges,
  refreshing
}: EdgeListViewProps) {
  return (
    <div className="space-y-4">
      <Card className="border-slate-200/80 bg-white/80 backdrop-blur">
        <CardHeader className="pb-3">
          <div className="text-xs uppercase tracking-[0.24em] text-slate-500">Fleet Overview</div>
          <div className="mt-1 text-2xl font-semibold tracking-tight">Edges</div>
          <div className="mt-1 text-sm text-muted-foreground">Connected workers, health, and rollout status.</div>
        </CardHeader>
        <CardContent className="grid grid-cols-2 gap-3 sm:grid-cols-4">
          <div className="rounded-md border bg-background/70 p-3">
            <div className="text-xs uppercase tracking-wide text-muted-foreground">Total</div>
            <div className="text-xl font-semibold">{edgeStats.total}</div>
          </div>
          <div className="rounded-md border bg-background/70 p-3">
            <div className="text-xs uppercase tracking-wide text-muted-foreground">Healthy</div>
            <div className="text-xl font-semibold text-emerald-600">{edgeStats.healthy}</div>
          </div>
          <div className="rounded-md border bg-background/70 p-3">
            <div className="text-xs uppercase tracking-wide text-muted-foreground">Degraded</div>
            <div className="text-xl font-semibold text-amber-600">{edgeStats.degraded}</div>
          </div>
          <div className="rounded-md border bg-background/70 p-3">
            <div className="text-xs uppercase tracking-wide text-muted-foreground">Pending Cmds</div>
            <div className="text-xl font-semibold">{formatNumber(edgeStats.pending)}</div>
          </div>
        </CardContent>
      </Card>

      <Card className="border-slate-200/80 bg-white/80 backdrop-blur">
        <CardHeader className="pb-3">
          <div className="flex items-center justify-end gap-3">
            <Button
              type="button"
              variant="outline"
              size="sm"
              className="h-9"
              onClick={onRefreshEdges}
              disabled={refreshing}
            >
              <RefreshCw className={`mr-1.5 h-4 w-4 ${refreshing ? "animate-spin" : ""}`} />
              {refreshing ? "Refreshing..." : "Refresh"}
            </Button>
            <div className="relative w-full max-w-[320px]">
              <Input
                value={edgeSearch}
                onChange={(event) => onEdgeSearchChange(event.target.value)}
                placeholder="Search edge name..."
                className="h-9 pl-8"
              />
              <Activity className="pointer-events-none absolute left-2.5 top-2.5 h-4 w-4 text-muted-foreground" />
            </div>
          </div>
        </CardHeader>
        <CardContent>
          <div className="overflow-hidden rounded-lg border">
            <div className="grid grid-cols-[minmax(160px,1fr)_130px_180px_230px_120px] gap-2 border-b bg-muted/40 px-3 py-2 text-[11px] uppercase tracking-wide text-muted-foreground">
              <div>Edge</div>
              <div>Sync</div>
              <div>Last Seen</div>
              <div>Applied Program</div>
              <div>Health</div>
            </div>
            <div className="max-h-[66vh] overflow-auto">
              {filteredEdges.map((edge) => {
                const health = edgeHealth(edge);
                return (
                  <button
                    key={edge.edge_id}
                    type="button"
                    onClick={() => onSelectEdge(edge.edge_id)}
                    className="grid w-full grid-cols-[minmax(160px,1fr)_130px_180px_230px_120px] items-center gap-2 border-b px-3 py-2 text-left text-sm transition hover:bg-muted/50"
                  >
                    <div className="flex items-center gap-2 font-medium">
                      <ChevronRight className="h-4 w-4 text-muted-foreground" />
                      <span className="truncate">{edge.edge_name}</span>
                    </div>
                    <div className={`text-xs font-semibold uppercase ${syncStatusClasses(edge.sync_status)}`}>
                      {edge.sync_status.split("_").join(" ")}
                    </div>
                    <div className="text-sm">{formatUnixMs(edge.last_seen_unix_ms)}</div>
                    <div className="min-w-0 text-sm">
                      {edge.applied_program ? (
                        <div className="flex items-center gap-1.5">
                          <span className="truncate">{edge.applied_program.name}</span>
                          <Badge className="rounded-full px-2 py-0 text-[10px] font-semibold uppercase tracking-wide">
                            v{edge.applied_program.version}
                          </Badge>
                        </div>
                      ) : (
                        "none"
                      )}
                    </div>
                    <div className={`flex items-center gap-1.5 text-xs uppercase ${edgeHealthClasses(edge)}`}>
                      <Circle className="h-3.5 w-3.5 fill-current" />
                      <span>{health}</span>
                    </div>
                  </button>
                );
              })}
              {filteredEdges.length === 0 ? (
                <div className="px-3 py-6 text-center text-sm text-muted-foreground">No edges match your search.</div>
              ) : null}
            </div>
          </div>
        </CardContent>
      </Card>
    </div>
  );
}
