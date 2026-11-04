import type { HistoryBreakdown } from "@/features/metrics/history-series";
import { type MetricRowChart, MetricRowHistory } from "@/features/metrics/metric-row-history";
import type { MetricPanel } from "@/features/metrics/use-metric-panel";

import { modelMemoryLines, ramHistory, vramHistory } from "./memory-history";
import type { MemoryKind } from "./site-memory";
import type { MemoryHistory } from "./use-memory-history";

/**
 * The expanded body of a RAM or VRAM row on the machines and models pages: used memory against
 * capacity, or one line per machine, plus one line per model deployment measured in that row. It
 * reads the panel's group named after the row's memory kind, as `memoryQueryGroups` builds it.
 */
export function MemoryRowHistory({
  breakdown,
  history,
  kind,
  panel,
}: {
  breakdown: HistoryBreakdown;
  history: MemoryHistory;
  kind: MemoryKind;
  panel: MetricPanel;
}) {
  return (
    <MetricRowHistory
      chart={memoryChart(kind, panel.history(kind) ?? [], history, breakdown)}
      label={kind === "ram" ? "RAM history" : "VRAM history"}
      panel={panel}
      unit="bytes"
    />
  );
}

/** The row's chart once its memory grids are read; the model grid is optional. */
function memoryChart(
  kind: MemoryKind,
  grids: readonly MetricRowChart["grid"][],
  { described, machineNames, models }: MemoryHistory,
  breakdown: HistoryBreakdown,
): MetricRowChart | undefined {
  const [first, second, modelGrid] = grids;
  if (!first || !second) return undefined;

  const series =
    kind === "ram"
      ? ramHistory(first, second, machineNames, breakdown)
      : vramHistory(first, second, described, machineNames, breakdown);

  return {
    grid: first,
    series: [
      ...series,
      ...(modelGrid ? modelMemoryLines(first, modelGrid, described, models, kind) : []),
    ],
  };
}
