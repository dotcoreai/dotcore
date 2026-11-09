import type { HistoryBreakdown } from "@/features/metrics/history-series";
import { type MetricRowChart, MetricRowHistory } from "@/features/metrics/metric-row-history";
import type { MetricPanel } from "@/features/metrics/use-metric-panel";

import { ramHistory, vramHistory } from "./memory-history";
import type { MemoryKind } from "./site-memory";
import type { MemoryHistory } from "./use-memory-history";

/**
 * The expanded body of a RAM or VRAM row on the machines page: used memory against capacity, or
 * one line per machine. It reads the panel's group named after the row's memory kind, as
 * `memoryQueryGroups` builds it.
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

/** The row's chart once its memory grids are read. */
function memoryChart(
  kind: MemoryKind,
  [first, second]: readonly MetricRowChart["grid"][],
  { described, machineNames }: MemoryHistory,
  breakdown: HistoryBreakdown,
): MetricRowChart | undefined {
  if (!first || !second) return undefined;

  return {
    grid: first,
    series:
      kind === "ram"
        ? ramHistory(first, second, machineNames, breakdown)
        : vramHistory(first, second, described, machineNames, breakdown),
  };
}
