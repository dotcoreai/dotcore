import "uplot/dist/uPlot.min.css";

import { type ReactNode, useEffect, useMemo, useRef, useState } from "react";
import type uPlot from "uplot";

import { useResourceColors } from "@/app/resource-color-context";
import { cn } from "@/lib/utils";

import {
  buildChartData,
  chartAxisIncrements,
  type ChartColor,
  type ChartData,
  type ChartSeries,
  type ChartUnit,
  formatChartAxisValue,
  formatChartValue,
  formatTime,
  seriesColorValue,
} from "./chart-data";

export interface TimeSeriesChartProps {
  className?: string;
  /** Pixel height of the plot; the width follows the container. */
  height?: number;
  /**
   * When the data source's history begins. Drawn as a marker when it falls inside the chart, so
   * time before it reads as "not recorded" rather than "idle".
   */
  historyStartMs?: number;
  /** Accessible name of the chart, such as "Host memory". */
  label: string;
  series: readonly ChartSeries[];
  /** Epoch milliseconds of the first grid step. */
  startMs: number;
  /** Grid steps the time axis spans at least, so a chart without data still spans its window. */
  stepCount?: number;
  stepMs: number;
  /** Shown in the readout line while the chart has no series, such as a loading status. */
  status?: ReactNode;
  unit: ChartUnit;
}

/**
 * A chart of one or more series over a regular time grid, each drawn as a line or as bars, which
 * may stack. Absent points break the line or leave no bar instead of reading as zero, and open
 * (provisional) points are dashed or faded. The readout above the plot shows each series' own
 * value, never a stacked total, at the hovered step, with its time, or, when idle, at the latest
 * step with data. The plot is a canvas, so the readout and the `img` label carry the same
 * information in text. Without series it draws its axes and grid over the window, so it holds its
 * place while data loads.
 */
export function TimeSeriesChart({
  className,
  height = 144,
  historyStartMs,
  label,
  series,
  startMs,
  stepCount,
  stepMs,
  status,
  unit,
}: TimeSeriesChartProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const plotRef = useRef<uPlot>(null);
  const historyStartRef = useRef(historyStartMs);
  const stepMsRef = useRef(stepMs);
  const [hoverIndex, setHoverIndex] = useState<number | null>(null);
  const dark = useDarkTheme();
  const { colorOf } = useResourceColors();

  const data = useMemo(
    () => buildChartData(startMs, stepMs, series, stepCount),
    [startMs, stepCount, stepMs, series],
  );
  const dataRef = useRef(data);
  const endMs = startMs + (data.x.length ? data.x.length * stepMs : 0);
  const historyStartVisible =
    historyStartMs !== undefined && historyStartMs > startMs && historyStartMs < endMs;
  // The plot is rebuilt only when its lines change, never for new values.
  // A range longer than a day repeats clock times, so its times carry the date.
  const withDate = data.x.length * stepMs > 24 * 60 * 60 * 1000;
  const structureKey = JSON.stringify(
    series.map(({ color, dashed, id, label, mark }) => ({ color, dashed, id, label, mark })),
  );
  const structure = useMemo(() => JSON.parse(structureKey) as ChartStructure[], [structureKey]);

  useEffect(() => {
    dataRef.current = data;
    stepMsRef.current = stepMs;
    historyStartRef.current = historyStartVisible ? historyStartMs : undefined;
    // Also redraws the history-start marker. A `redraw()` after it would pin the time axis to the
    // previous window, as uPlot re-applies the current x scale over the one new data rescales to.
    plotRef.current?.setData([data.x, ...data.columns]);
  }, [data, historyStartMs, historyStartVisible, stepMs]);

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    let cancelled = false;
    let teardown: (() => void) | undefined;

    // uPlot reads `matchMedia` when it loads, so it is imported only once a chart needs it.
    void import("uplot").then(({ default: uPlot }) => {
      if (cancelled) return;

      const plot = new uPlot(
        chartOptions({
          colors: resolveColors(container, structure, colorOf),
          data: () => dataRef.current,
          height,
          historyStartMs: () => historyStartRef.current,
          onCursor: setHoverIndex,
          paths: uPlot.paths,
          series: structure,
          stepMs: () => stepMsRef.current,
          unit,
          width: container.clientWidth,
        }),
        [dataRef.current.x, ...dataRef.current.columns],
        container,
      );
      plotRef.current = plot;

      const observer =
        typeof ResizeObserver === "undefined"
          ? undefined
          : new ResizeObserver(() => {
              if (container.clientWidth > 0) plot.setSize({ width: container.clientWidth, height });
            });
      observer?.observe(container);

      teardown = () => {
        observer?.disconnect();
        plot.destroy();
        plotRef.current = null;
      };
    });

    return () => {
      cancelled = true;
      teardown?.();
    };
  }, [structure, unit, height, dark, colorOf]);

  const readoutIndex = hoverIndex ?? readoutStep(series);
  const readoutSeries = series.map((entry) => ({
    entry,
    value: readoutIndex === null ? null : (entry.values[readoutIndex] ?? null),
  }));

  return (
    <div className={cn("min-w-0", className)} role="group" aria-label={label}>
      <div className="mb-1 flex min-h-5 flex-wrap items-baseline gap-x-3 gap-y-0.5 text-xs tabular-nums">
        {/* Idle, the values read as current, so only a hovered step names its time. */}
        {hoverIndex === null ? null : (
          <span className="text-muted-foreground">
            {formatTime(startMs + hoverIndex * stepMs, withDate)}
          </span>
        )}
        {readoutSeries.map(({ entry, value }) => (
          <span className="inline-flex items-baseline gap-1.5" key={entry.id}>
            <span
              aria-hidden="true"
              className={cn(
                "size-2 self-center rounded-full",
                typeof entry.color === "string" && swatchClass[entry.color],
              )}
              data-slot="chart-swatch"
              style={
                typeof entry.color === "object"
                  ? { backgroundColor: seriesColorValue(entry.color, colorOf) }
                  : undefined
              }
            />
            <span className="text-muted-foreground">{entry.label}</span>
            <span>{value === null ? "No data" : formatChartValue(unit, value)}</span>
          </span>
        ))}
        {series.length || !status ? null : <span className="text-muted-foreground">{status}</span>}
      </div>
      <div
        aria-label={`${label} plot`}
        className="w-full"
        ref={containerRef}
        role="img"
        style={{ minHeight: height }}
      />
    </div>
  );
}

const swatchClass: Record<ChartColor, string> = {
  "chart-1": "bg-chart-1",
  "chart-2": "bg-chart-2",
  "chart-3": "bg-chart-3",
  "chart-4": "bg-chart-4",
  "chart-5": "bg-chart-5",
  muted: "bg-muted-foreground",
};

const colorVariable: Record<ChartColor, string> = {
  "chart-1": "--chart-1",
  "chart-2": "--chart-2",
  "chart-3": "--chart-3",
  "chart-4": "--chart-4",
  "chart-5": "--chart-5",
  muted: "--muted-foreground",
};

/** The parts of a series that decide how it is drawn, without its values. */
type ChartStructure = Pick<ChartSeries, "color" | "dashed" | "id" | "label" | "mark">;

interface ResolvedColors {
  axis: string;
  grid: string;
  marker: string;
  series: string[];
}

interface ChartOptionsInput {
  colors: ResolvedColors;
  /** The plot's current data, read on every draw since stacked bar bases change with values. */
  data: () => ChartData;
  height: number;
  historyStartMs: () => number | undefined;
  onCursor: (index: number | null) => void;
  paths: uPlot.Series.PathBuilderFactories;
  series: readonly ChartStructure[];
  stepMs: () => number;
  unit: ChartUnit;
  width: number;
}

function chartOptions({
  colors,
  data,
  height,
  historyStartMs,
  onCursor,
  paths,
  series,
  stepMs,
  unit,
  width,
}: ChartOptionsInput): uPlot.Options {
  const line = (entry: ChartStructure, index: number, overlay: boolean): uPlot.Series => ({
    label: entry.label,
    stroke: colors.series[index],
    width: 1.5,
    dash: overlay || entry.dashed ? [4, 3] : undefined,
    spanGaps: false,
    points: {
      show: false,
      size: 4,
      fill: colors.series[index],
      // A lone point between gaps has no line to draw, so it is shown as a dot.
      filter: (plot, seriesIndex) => isolatedIndexes(plot.data[seriesIndex]),
    },
  });
  const bars = (entry: ChartStructure, index: number, overlay: boolean): uPlot.Series => ({
    label: entry.label,
    stroke: colors.series[index],
    fill: colors.series[index],
    width: 0,
    // Open bars count a bucket that is still filling, so they are faded rather than dashed.
    alpha: overlay ? 0.45 : 1,
    paths: paths.bars?.({
      size: [0.8, Infinity, 1],
      // Stacked bars rise from the total below them rather than from zero.
      disp:
        entry.mark === "stackedBars"
          ? {
              y0: {
                unit: scaleValueUnit,
                values: (_plot, seriesIndex) => data().bases[seriesIndex - 1] ?? [],
              },
              y1: {
                unit: scaleValueUnit,
                values: (_plot, seriesIndex) => data().columns[seriesIndex - 1] ?? [],
              },
            }
          : undefined,
    }),
    points: { show: false },
  });
  const draw = (entry: ChartStructure, index: number, overlay: boolean) =>
    entry.mark === "bars" || entry.mark === "stackedBars"
      ? bars(entry, index, overlay)
      : line(entry, index, overlay);
  const hasBars = series.some((entry) => entry.mark === "bars" || entry.mark === "stackedBars");
  const axisStyle = {
    stroke: colors.axis,
    grid: { stroke: colors.grid, width: 1 },
    ticks: { stroke: colors.grid, width: 1 },
  };

  return {
    width,
    height,
    legend: { show: false },
    cursor: { drag: { x: false, y: false }, points: { size: 6 } },
    scales: {
      x: {
        time: true,
        // Bars are centred on their steps, so the axis spans half a step past each end.
        range: hasBars
          ? (_plot, min, max) => [min - stepMs() / 2000, max + stepMs() / 2000]
          : undefined,
      },
      y: {
        // A ratio is a share of capacity, so its axis always spans the whole of it.
        range: (_plot, _min, max) =>
          unit === "ratio" ? [0, 1] : [0, Number.isFinite(max) && max > 0 ? max * 1.05 : 1],
      },
    },
    axes: [
      { ...axisStyle, size: 28 },
      {
        ...axisStyle,
        incrs: chartAxisIncrements(unit),
        size: 64,
        values: (_plot, splits) => splits.map((value) => formatChartAxisValue(unit, value)),
      },
    ],
    series: [
      {},
      ...series.map((entry, index) => draw(entry, index, false)),
      ...series.map((entry, index) => ({
        ...draw(entry, index, true),
        // Overlay segments continue their series, so only the solid series is named.
        label: `${entry.label} (open)`,
      })),
    ],
    hooks: {
      setCursor: [(plot) => onCursor(plot.cursor.idx ?? null)],
      draw: [(plot) => drawHistoryStart(plot, historyStartMs(), colors)],
    },
  };
}

/** uPlot's bar facet unit for values on the y scale; a const enum, so it cannot be imported. */
const scaleValueUnit = 1 as uPlot.Series.BarsPathBuilderFacetUnit;

/** Shades the unrecorded span before `startMs` and marks its end with a vertical line. */
function drawHistoryStart(plot: uPlot, startMs: number | undefined, colors: ResolvedColors) {
  if (startMs === undefined) return;

  const { ctx, bbox } = plot;
  const left = plot.valToPos(startMs / 1000, "x", true);
  if (left <= bbox.left || left >= bbox.left + bbox.width) return;

  ctx.save();
  ctx.globalAlpha = 0.08;
  ctx.fillStyle = colors.marker;
  ctx.fillRect(bbox.left, bbox.top, left - bbox.left, bbox.height);
  ctx.globalAlpha = 1;
  ctx.strokeStyle = colors.marker;
  ctx.lineWidth = 1;
  ctx.setLineDash([4, 3]);
  ctx.beginPath();
  ctx.moveTo(left, bbox.top);
  ctx.lineTo(left, bbox.top + bbox.height);
  ctx.stroke();
  ctx.restore();
}

function isolatedIndexes(column: ArrayLike<number | null | undefined>): number[] | null {
  const indexes: number[] = [];
  for (let index = 0; index < column.length; index += 1) {
    if (column[index] == null) continue;
    if (column[index - 1] == null && column[index + 1] == null) indexes.push(index);
  }
  return indexes.length ? indexes : null;
}

function resolveColors(
  container: HTMLElement,
  series: readonly ChartStructure[],
  colorOf: (key: string) => string,
): ResolvedColors {
  const style = getComputedStyle(container);
  const token = (name: string) => style.getPropertyValue(name).trim() || "currentColor";
  // A canvas cannot resolve custom properties, so tokens are read from the computed style.
  const seriesColors = series.map((entry) =>
    typeof entry.color === "object"
      ? seriesColorValue(entry.color, colorOf)
      : token(colorVariable[entry.color]),
  );

  return {
    axis: token("--muted-foreground"),
    grid: token("--border"),
    marker: token("--muted-foreground"),
    series: seriesColors,
  };
}

/** Whether the document is in the dark theme; canvas colors are re-resolved when it flips. */
function useDarkTheme(): boolean {
  const [dark, setDark] = useState(() => document.documentElement.classList.contains("dark"));

  useEffect(() => {
    const observer = new MutationObserver(() =>
      setDark(document.documentElement.classList.contains("dark")),
    );
    observer.observe(document.documentElement, { attributeFilter: ["class"], attributes: true });
    return () => observer.disconnect();
  }, []);

  return dark;
}

/**
 * The grid step the idle readout reads: the newest step where some series has a value and no
 * series is provisional, so a step still filling in from some sources is never read as the
 * current value. Falls back to the newest step with a value when none has closed, or null when
 * every series is empty.
 */
function readoutStep(series: readonly ChartSeries[]): number | null {
  const length = series.reduce((longest, entry) => Math.max(longest, entry.values.length), 0);
  let newestWithValue: number | null = null;
  for (let index = length - 1; index >= 0; index -= 1) {
    if (!series.some((entry) => entry.values[index] != null)) continue;
    if (!series.some((entry) => entry.provisional?.[index])) return index;
    newestWithValue ??= index;
  }
  return newestWithValue;
}
