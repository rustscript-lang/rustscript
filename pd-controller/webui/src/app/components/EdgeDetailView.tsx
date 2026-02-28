import { ArrowLeft } from "lucide-react";

import { formatUnixMs } from "@/app/helpers";
import { MultiLineChart } from "@/app/components/charts";
import type { EdgeDetailResponse, EdgeTrafficPoint, ProgramSummary } from "@/app/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader } from "@/components/ui/card";
import { Label } from "@/components/ui/label";

type EdgeDetailViewProps = {
  selectedEdge: EdgeDetailResponse | null;
  onBack: () => void;
  programs: ProgramSummary[];
  applyProgramId: string;
  onApplyProgramChange: (programId: string) => void;
  applyVersion: string;
  onApplyVersionChange: (version: string) => void;
  selectedApplyProgram: ProgramSummary | null;
  applyLoading: boolean;
  applyStatus: string;
  onApplyProgram: () => void;
};

function statusRatePerSecond(
  point: EdgeTrafficPoint,
  index: number,
  points: EdgeTrafficPoint[],
  selector: (item: EdgeTrafficPoint) => number
): number {
  if (index <= 0) {
    return 0;
  }
  const previous = points[index - 1];
  const deltaMs = point.unix_ms - previous.unix_ms;
  if (deltaMs <= 0) {
    return 0;
  }
  return (selector(point) * 1_000) / deltaMs;
}

function statusRatePerMinute(
  point: EdgeTrafficPoint,
  index: number,
  points: EdgeTrafficPoint[],
  selector: (item: EdgeTrafficPoint) => number
): number {
  return statusRatePerSecond(point, index, points, selector) * 60;
}

export function EdgeDetailView({
  selectedEdge,
  onBack,
  programs,
  applyProgramId,
  onApplyProgramChange,
  applyVersion,
  onApplyVersionChange,
  selectedApplyProgram,
  applyLoading,
  applyStatus,
  onApplyProgram
}: EdgeDetailViewProps) {
  return (
    <div className="space-y-4">
      <Card className="border-slate-200/80 bg-white/80 backdrop-blur">
        <CardHeader>
          <div className="flex items-center justify-between gap-3">
            <div>
              <div className="text-xs uppercase tracking-[0.24em] text-slate-500">Fleet Workspace</div>
              <div className="mt-1 text-2xl font-semibold tracking-tight">Edge Detail</div>
              <div className="mt-1 text-sm text-muted-foreground">
                {selectedEdge ? selectedEdge.summary.edge_name : "No edge selected"}
              </div>
            </div>
            <Button variant="outline" onClick={onBack} className="inline-flex items-center gap-1">
              <ArrowLeft className="h-4 w-4" />
              Back To Edges
            </Button>
          </div>
        </CardHeader>
        <CardContent>
          {selectedEdge ? (
            <div className="space-y-4">
              <div className="grid grid-cols-2 gap-2">
                <div className="rounded-md border bg-background/70 p-2">
                  <div className="text-[11px] uppercase tracking-wide text-muted-foreground">Pending</div>
                  <div className="text-lg font-semibold">{selectedEdge.summary.pending_commands}</div>
                </div>
                <div className="rounded-md border bg-background/70 p-2">
                  <div className="text-[11px] uppercase tracking-wide text-muted-foreground">Recent Results</div>
                  <div className="text-lg font-semibold">{selectedEdge.summary.recent_results}</div>
                </div>
                <div className="rounded-md border bg-background/70 p-2">
                  <div className="text-[11px] uppercase tracking-wide text-muted-foreground">Last Poll</div>
                  <div className="text-sm">{formatUnixMs(selectedEdge.summary.last_poll_unix_ms)}</div>
                </div>
                <div className="rounded-md border bg-background/70 p-2">
                  <div className="text-[11px] uppercase tracking-wide text-muted-foreground">Last Result</div>
                  <div className="text-sm">{formatUnixMs(selectedEdge.summary.last_result_unix_ms)}</div>
                </div>
              </div>
              <div className="rounded-md border bg-background/70 p-2">
                <div className="text-[11px] uppercase tracking-wide text-muted-foreground">Edge UUID</div>
                <div className="break-all font-mono text-xs">{selectedEdge.summary.edge_id}</div>
              </div>
              <div className="rounded-md border bg-background/70 p-2">
                <div className="text-[11px] uppercase tracking-wide text-muted-foreground">Currently Applied Program</div>
                <div className="text-sm font-semibold">
                  {selectedEdge.summary.applied_program ? (
                    <div className="flex items-center gap-1.5">
                      <span>{selectedEdge.summary.applied_program.name}</span>
                      <Badge className="rounded-full px-2 py-0 text-[10px] font-semibold uppercase tracking-wide">
                        v{selectedEdge.summary.applied_program.version}
                      </Badge>
                    </div>
                  ) : (
                    "none"
                  )}
                </div>
              </div>
              <div className="space-y-2">
                <div className="text-sm font-semibold">Traffic Over Time</div>
                <div className="grid grid-cols-1 gap-3 xl:grid-cols-2">
                  <div>
                    <div className="mb-1 text-xs uppercase tracking-wide text-muted-foreground">Latency Percentiles</div>
                    <MultiLineChart
                      points={selectedEdge.traffic_series}
                      series={[
                        { key: "p50", stroke: "#0ea5e9", valueFor: (point) => point.latency_p50_ms },
                        { key: "p90", stroke: "#f59e0b", valueFor: (point) => point.latency_p90_ms },
                        { key: "p99", stroke: "#dc2626", valueFor: (point) => point.latency_p99_ms }
                      ]}
                      emptyLabel="No latency samples yet."
                      xAxisLabel="Time"
                      yAxisLabel="Latency (ms)"
                    />
                  </div>
                  <div className="space-y-2">
                    <div className="mb-1 text-xs uppercase tracking-wide text-muted-foreground">Status Codes / Second</div>
                    <MultiLineChart
                      points={selectedEdge.traffic_series}
                      series={[
                        {
                          key: "2xx",
                          stroke: "#16a34a",
                          valueFor: (point, index, points) => statusRatePerSecond(point, index, points, (item) => item.status_2xx)
                        },
                        {
                          key: "3xx",
                          stroke: "#0ea5e9",
                          valueFor: (point, index, points) => statusRatePerSecond(point, index, points, (item) => item.status_3xx)
                        },
                        {
                          key: "4xx",
                          stroke: "#f59e0b",
                          valueFor: (point, index, points) => statusRatePerSecond(point, index, points, (item) => item.status_4xx)
                        },
                        {
                          key: "5xx",
                          stroke: "#dc2626",
                          valueFor: (point, index, points) => statusRatePerSecond(point, index, points, (item) => item.status_5xx)
                        }
                      ]}
                      hideZeroSeries
                      emptyLabel="No status samples per second yet."
                      xAxisLabel="Time"
                      yAxisLabel="Requests / sec"
                    />
                    <div className="mb-1 text-xs uppercase tracking-wide text-muted-foreground">Status Codes / Minute</div>
                    <MultiLineChart
                      points={selectedEdge.traffic_series}
                      series={[
                        {
                          key: "2xx",
                          stroke: "#16a34a",
                          valueFor: (point, index, points) => statusRatePerMinute(point, index, points, (item) => item.status_2xx)
                        },
                        {
                          key: "3xx",
                          stroke: "#0ea5e9",
                          valueFor: (point, index, points) => statusRatePerMinute(point, index, points, (item) => item.status_3xx)
                        },
                        {
                          key: "4xx",
                          stroke: "#f59e0b",
                          valueFor: (point, index, points) => statusRatePerMinute(point, index, points, (item) => item.status_4xx)
                        },
                        {
                          key: "5xx",
                          stroke: "#dc2626",
                          valueFor: (point, index, points) => statusRatePerMinute(point, index, points, (item) => item.status_5xx)
                        }
                      ]}
                      hideZeroSeries
                      emptyLabel="No status samples per minute yet."
                      xAxisLabel="Time"
                      yAxisLabel="Requests / min"
                    />
                  </div>
                </div>
              </div>

              {selectedEdge.summary.last_telemetry ? (
                <div className="rounded-md border bg-background/70 p-3 text-xs">
                  <div className="mb-2 text-[11px] uppercase tracking-wide text-muted-foreground">Telemetry Snapshot</div>
                  <div className="grid grid-cols-1 gap-1 sm:grid-cols-2">
                    <div>uptime_seconds: {selectedEdge.summary.last_telemetry.uptime_seconds}</div>
                    <div>program_loaded: {String(selectedEdge.summary.last_telemetry.program_loaded)}</div>
                    <div>debug_session_active: {String(selectedEdge.summary.last_telemetry.debug_session_active)}</div>
                    <div>data_requests_total: {selectedEdge.summary.last_telemetry.data_requests_total}</div>
                    <div>vm_execution_errors_total: {selectedEdge.summary.last_telemetry.vm_execution_errors_total}</div>
                    <div>program_apply_success_total: {selectedEdge.summary.last_telemetry.program_apply_success_total}</div>
                  </div>
                </div>
              ) : (
                <div className="rounded-md border bg-background/70 p-3 text-sm text-muted-foreground">
                  No telemetry has been reported for this edge yet.
                </div>
              )}

              <div className="rounded-md border bg-background/70 p-3">
                <div className="mb-2 text-sm font-semibold">Apply Program</div>
                <div className="grid grid-cols-1 gap-2">
                  <div className="space-y-1">
                    <Label htmlFor="apply-program">Program</Label>
                    <select
                      id="apply-program"
                      value={applyProgramId}
                      onChange={(event) => onApplyProgramChange(event.target.value)}
                      className="h-9 w-full rounded-md border bg-background px-2 text-sm"
                    >
                      <option value="">Select program</option>
                      {programs.map((program) => (
                        <option key={program.program_id} value={program.program_id} disabled={program.latest_version === 0}>
                          {program.name} {program.latest_version === 0 ? "(v0 draft)" : ""}
                        </option>
                      ))}
                    </select>
                  </div>
                  <div className="space-y-1">
                    <Label htmlFor="apply-version">Version</Label>
                    <select
                      id="apply-version"
                      value={applyVersion}
                      onChange={(event) => onApplyVersionChange(event.target.value)}
                      className="h-9 w-full rounded-md border bg-background px-2 text-sm"
                    >
                      <option value="latest">latest</option>
                      {selectedApplyProgram && selectedApplyProgram.latest_version > 0
                        ? Array.from({ length: selectedApplyProgram.latest_version }, (_, index) => index + 1)
                            .reverse()
                            .map((version) => (
                              <option key={version} value={String(version)}>
                                v{version}
                              </option>
                            ))
                        : null}
                    </select>
                  </div>
                  <Button
                    onClick={onApplyProgram}
                    disabled={applyLoading || !applyProgramId || (selectedApplyProgram?.latest_version ?? 0) === 0}
                  >
                    {applyLoading ? "Applying" : "Apply To Edge"}
                  </Button>
                  {selectedApplyProgram && selectedApplyProgram.latest_version === 0 ? (
                    <div className="text-xs text-amber-600">This program is still draft v0. Save a version first.</div>
                  ) : null}
                  {applyStatus ? <div className="text-xs text-muted-foreground">{applyStatus}</div> : null}
                </div>
              </div>
            </div>
          ) : (
            <div className="rounded-md border bg-background/70 p-4 text-sm text-muted-foreground">
              Select a edge from the list first.
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
