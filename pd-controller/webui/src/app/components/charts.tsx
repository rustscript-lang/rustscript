import type { LineSeries } from "@/app/helpers";
import type { EdgeTrafficPoint } from "@/app/types";

const WIDTH = 520;
const HEIGHT = 180;
const PLOT_LEFT = 44;
const PLOT_RIGHT = 12;
const PLOT_TOP = 8;
const PLOT_BOTTOM = 30;

type AxisProps = {
  xAxisLabel?: string;
  yAxisLabel?: string;
};

function formatAxisTime(unixMs: number): string {
  return new Date(unixMs).toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false
  });
}

function axisAndScales(points: EdgeTrafficPoint[], maxYInput: number) {
  const maxY = Number.isFinite(maxYInput) && maxYInput > 0 ? maxYInput : 1;
  const plotWidth = WIDTH - PLOT_LEFT - PLOT_RIGHT;
  const plotHeight = HEIGHT - PLOT_TOP - PLOT_BOTTOM;
  const xStep = points.length > 1 ? plotWidth / (points.length - 1) : 0;
  const ticksY = [maxY, maxY / 2, 0];
  return { maxY, plotWidth, plotHeight, xStep, ticksY };
}

function xFor(index: number, xStep: number): number {
  return PLOT_LEFT + index * xStep;
}

function yFor(value: number, maxY: number, plotHeight: number): number {
  return PLOT_TOP + (1 - value / maxY) * plotHeight;
}

function safeChartValue(value: number): number {
  if (!Number.isFinite(value) || value < 0) {
    return 0;
  }
  return value;
}

function formatAxisValue(value: number): string {
  if (!Number.isFinite(value)) {
    return "0";
  }
  const absValue = Math.abs(value);
  const maximumFractionDigits = absValue >= 100 ? 0 : absValue >= 10 ? 1 : 2;
  return new Intl.NumberFormat(undefined, { maximumFractionDigits }).format(value);
}

function pathForPoints(
  points: EdgeTrafficPoint[],
  valueFor: (point: EdgeTrafficPoint, index: number, points: EdgeTrafficPoint[]) => number,
  maxY: number,
  plotHeight: number,
  xStep: number
): string {
  return points
    .map((point, index) => {
      const x = xFor(index, xStep);
      const value = safeChartValue(valueFor(point, index, points));
      const y = yFor(value, maxY, plotHeight);
      return `${index === 0 ? "M" : "L"} ${x.toFixed(1)} ${y.toFixed(1)}`;
    })
    .join(" ");
}

function AxisFrame({
  points,
  maxY,
  ticksY,
  xAxisLabel = "Time",
  yAxisLabel = "Value"
}: {
  points: EdgeTrafficPoint[];
  maxY: number;
  ticksY: number[];
} & AxisProps) {
  const plotHeight = HEIGHT - PLOT_TOP - PLOT_BOTTOM;
  return (
    <>
      <line x1={PLOT_LEFT} y1={PLOT_TOP} x2={PLOT_LEFT} y2={PLOT_TOP + plotHeight} stroke="#cbd5e1" strokeWidth={1} />
      <line
        x1={PLOT_LEFT}
        y1={PLOT_TOP + plotHeight}
        x2={WIDTH - PLOT_RIGHT}
        y2={PLOT_TOP + plotHeight}
        stroke="#cbd5e1"
        strokeWidth={1}
      />
      {ticksY.map((tick) => {
        const y = yFor(tick, maxY, plotHeight);
        return (
          <g key={`tick-${tick}`}>
            <line x1={PLOT_LEFT} y1={y} x2={WIDTH - PLOT_RIGHT} y2={y} stroke="#e2e8f0" strokeWidth={1} />
            <text x={PLOT_LEFT - 6} y={y + 4} textAnchor="end" fontSize={10} fill="#64748b">
              {formatAxisValue(tick)}
            </text>
          </g>
        );
      })}
      {points.length > 0 ? (
        <>
          <text x={PLOT_LEFT} y={HEIGHT - 8} textAnchor="start" fontSize={10} fill="#64748b">
            {formatAxisTime(points[0].unix_ms)}
          </text>
          <text x={WIDTH - PLOT_RIGHT} y={HEIGHT - 8} textAnchor="end" fontSize={10} fill="#64748b">
            {formatAxisTime(points[points.length - 1].unix_ms)}
          </text>
        </>
      ) : null}
      <text x={(PLOT_LEFT + WIDTH - PLOT_RIGHT) / 2} y={HEIGHT - 2} textAnchor="middle" fontSize={10} fill="#64748b">
        {xAxisLabel}
      </text>
      <text x={12} y={HEIGHT / 2} textAnchor="middle" fontSize={10} fill="#64748b" transform={`rotate(-90 12 ${HEIGHT / 2})`}>
        {yAxisLabel}
      </text>
    </>
  );
}

export function LineChart({
  points,
  valueFor,
  stroke,
  emptyLabel,
  xAxisLabel = "Time",
  yAxisLabel = "Value"
}: {
  points: EdgeTrafficPoint[];
  valueFor: (point: EdgeTrafficPoint, index: number, points: EdgeTrafficPoint[]) => number;
  stroke: string;
  emptyLabel: string;
} & AxisProps) {
  if (points.length === 0) {
    return (
      <div className="h-[180px] rounded-md border bg-background/70 p-3 text-sm text-muted-foreground">
        {emptyLabel}
      </div>
    );
  }

  const maxYRaw = Math.max(...points.map((point, index) => safeChartValue(valueFor(point, index, points))), 1);
  const { maxY, plotHeight, xStep, ticksY } = axisAndScales(points, maxYRaw);
  const path = pathForPoints(points, valueFor, maxY, plotHeight, xStep);
  const latestValue = safeChartValue(valueFor(points[points.length - 1], points.length - 1, points));

  return (
    <div className="rounded-md border bg-background/70 p-3">
      <svg viewBox={`0 0 ${WIDTH} ${HEIGHT}`} className="h-[180px] w-full">
        <AxisFrame points={points} maxY={maxY} ticksY={ticksY} xAxisLabel={xAxisLabel} yAxisLabel={yAxisLabel} />
        <path d={path} fill="none" stroke={stroke} strokeWidth={2.5} />
      </svg>
      <div className="mt-2 text-xs text-muted-foreground">
        latest={formatAxisValue(latestValue)} max={formatAxisValue(maxY)}
      </div>
    </div>
  );
}

export function MultiLineChart({
  points,
  series,
  emptyLabel,
  hideZeroSeries = false,
  xAxisLabel = "Time",
  yAxisLabel = "Value"
}: {
  points: EdgeTrafficPoint[];
  series: LineSeries[];
  emptyLabel: string;
  hideZeroSeries?: boolean;
} & AxisProps) {
  const visibleSeries = hideZeroSeries
    ? series.filter((item) =>
        points.some((point, index) => safeChartValue(item.valueFor(point, index, points)) > 0)
      )
    : series;
  if (points.length === 0 || visibleSeries.length === 0) {
    return (
      <div className="h-[180px] rounded-md border bg-background/70 p-3 text-sm text-muted-foreground">
        {emptyLabel}
      </div>
    );
  }

  const maxYRaw = Math.max(
    ...points.flatMap((point, index) => visibleSeries.map((item) => safeChartValue(item.valueFor(point, index, points)))),
    1
  );
  const { maxY, plotHeight, xStep, ticksY } = axisAndScales(points, maxYRaw);
  const lines = visibleSeries.map((item) => ({
    ...item,
    path: pathForPoints(points, item.valueFor, maxY, plotHeight, xStep)
  }));

  return (
    <div className="rounded-md border bg-background/70 p-3">
      <svg viewBox={`0 0 ${WIDTH} ${HEIGHT}`} className="h-[180px] w-full">
        <AxisFrame points={points} maxY={maxY} ticksY={ticksY} xAxisLabel={xAxisLabel} yAxisLabel={yAxisLabel} />
        {lines.map((line) => (
          <path key={line.key} d={line.path} fill="none" stroke={line.stroke} strokeWidth={2.2} />
        ))}
      </svg>
      <div className="mt-2 flex flex-wrap gap-3 text-xs text-muted-foreground">
        {lines.map((line) => (
          <div key={`${line.key}-legend`} className="inline-flex items-center gap-1">
            <span className="inline-block h-2.5 w-2.5 rounded-full" style={{ background: line.stroke }} />
            {line.key}
          </div>
        ))}
      </div>
    </div>
  );
}
