import { useState } from "react";

import type { ChartSeries, ChartUnit } from "@/components/metrics/time-series-chart/chart-data";
import { TimeSeriesChart } from "@/components/metrics/time-series-chart/time-series-chart";
import { Notice } from "@/components/ui/notice";
import { SegmentedControl } from "@/components/ui/segmented-control";

import type { MetricGrid } from "./metric-grid";
import type { MetricRange } from "./metric-ranges";
import type { MetricPanel } from "./use-metric-panel";

/**
 * What an expanded row charts once its history is read: the grid that sets its time axis, and its
 * lines.
 */
export interface MetricRowChart {
  grid: MetricGrid;
  series: ChartSeries[];
}

/**
 * The expanded body of a metric row: the panel's range picker, unless the caller owns the range,
 * and the row's history chart. Until
 * history is read the chart draws empty over the selected range, saying it is loading only when
 * that takes a while; when history cannot be read, a notice takes its place.
 */
export function MetricRowHistory({
  chart,
  label,
  panel,
  rangePicker = true,
  unit,
}: {
  /** The row's chart; `undefined` until history is read. */
  chart: MetricRowChart | undefined;
  /** Accessible name of the chart, such as "RAM history". */
  label: string;
  panel: MetricPanel;
  /** Whether the row shows the panel's range picker; false when the caller owns the range. */
  rangePicker?: boolean;
  unit: ChartUnit;
}) {
  // The empty chart ends when the row opened; the first answer brings the real window.
  const [openedAtMs] = useState(() => Date.now());
  const rangesLoading = !panel.ranges.length && !panel.historyUnavailable;
  const empty = emptyWindow(panel.range ?? loadingRange, openedAtMs);

  return (
    <div className="flex flex-col gap-2">
      {!rangePicker ? null : panel.ranges.length > 1 ? (
        <SegmentedControl
          aria-label="History range"
          className="self-end"
          onValueChange={panel.setRange}
          options={panel.ranges.map((range) => ({ label: range.label, value: range.label }))}
          value={panel.range?.label}
        />
      ) : rangesLoading ? (
        // Holds the range picker's place, so the chart doesn't move down when the ranges are read.
        <div
          aria-hidden="true"
          className="box-content h-control-xs self-end border p-0.5 opacity-0"
        />
      ) : null}
      {chart ? (
        <>
          <TimeSeriesChart
            height={chartHeight}
            historyStartMs={chart.grid.incarnation.startedAtMs}
            label={label}
            series={chart.series}
            startMs={chart.grid.startMs}
            stepMs={chart.grid.stepMs}
            unit={unit}
          />
          {panel.historyFailed ? (
            <p className="text-muted-foreground" role="status">
              History update failed; showing the last values read.
            </p>
          ) : null}
        </>
      ) : panel.historyFailed || panel.historyUnavailable ? (
        <Notice tone="destructive">History could not be read.</Notice>
      ) : (
        <TimeSeriesChart
          height={chartHeight}
          label={label}
          series={[]}
          startMs={empty.startMs}
          status={
            panel.historyLoadingVisible ? <span role="status">Loading history…</span> : undefined
          }
          stepCount={empty.stepCount}
          stepMs={empty.stepMs}
          unit={unit}
        />
      )}
    </div>
  );
}

const chartHeight = 120;

/** The window an empty chart spans while the ranges are read: the store's finest, 15 min at 1 s. */
const loadingRange: Pick<MetricRange, "durationMs" | "stepMs"> = {
  durationMs: 15 * 60 * 1000,
  stepMs: 1000,
};

/** The step grid of `range` ending at `endMs`, aligned to whole steps like the store's buckets. */
function emptyWindow(range: Pick<MetricRange, "durationMs" | "stepMs">, endMs: number) {
  const stepCount = Math.max(1, Math.round(range.durationMs / range.stepMs));
  const lastStepMs = Math.floor(endMs / range.stepMs) * range.stepMs;

  return { startMs: lastStepMs - (stepCount - 1) * range.stepMs, stepCount, stepMs: range.stepMs };
}
