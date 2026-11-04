import {
  capacitySeries,
  type ChartSeries,
} from "@/components/metrics/time-series-chart/chart-data";

import type { MetricGrid, MetricGridSeries } from "./metric-grid";
import { dimensionNames } from "./metric-names";

/** How history is drawn: one combined series, or one series per machine in its marker color. */
export type HistoryBreakdown = "combined" | "machines";

/** One step series: a value per step, `null` where absent, and whether each step is open. */
export interface StepValues {
  provisional: boolean[];
  values: (number | null)[];
}

/** One reporter of a used reading, with the capacity it is read against, keyed by its machine. */
export interface CapacityMember {
  capacity: { provisional: readonly boolean[]; values: readonly (number | null)[] };
  key: string;
  used: { provisional: readonly boolean[]; values: readonly (number | null)[] };
}

/**
 * Used readings against their capacity. Combined, each step sums used and capacity over the
 * members reporting both, then draws the capacity dashed; per machine, each machine's used memory
 * is summed over its members and drawn in its marker color.
 */
export function usedAgainstCapacity(
  members: readonly CapacityMember[],
  stepCount: number,
  names: ReadonlyMap<string, string>,
  breakdown: HistoryBreakdown,
): ChartSeries[] {
  if (breakdown === "machines") {
    const byNode = new Map<string, CapacityMember[]>();
    for (const member of members)
      byNode.set(member.key, [...(byNode.get(member.key) ?? []), member]);

    return [...byNode].map(([node, nodeMembers]) => ({
      color: { resource: node },
      id: node,
      label: names.get(node) ?? node,
      ...sumPerStep(
        nodeMembers.map((member) => member.used),
        stepCount,
      ),
    }));
  }

  const used: StepValues = { provisional: [], values: [] };
  const capacity: StepValues = { provisional: [], values: [] };
  for (let index = 0; index < stepCount; index += 1) {
    let usedSum = 0;
    let capacitySum = 0;
    let reporting = false;
    let open = false;
    for (const member of members) {
      const usedValue = member.used.values[index];
      const capacityValue = member.capacity.values[index];
      if (usedValue == null || capacityValue == null) continue;
      usedSum += usedValue;
      capacitySum += capacityValue;
      reporting = true;
      open ||= Boolean(member.used.provisional[index] || member.capacity.provisional[index]);
    }
    used.values.push(reporting ? usedSum : null);
    capacity.values.push(reporting ? capacitySum : null);
    used.provisional.push(open);
    capacity.provisional.push(open);
  }

  return [
    { color: "chart-1", id: "used", label: "Used", ...used },
    capacitySeries(capacity.values, stepCount, capacity.provisional),
  ];
}

/** Per step, the sum of the members reporting in it; absent when none does. */
export function sumPerStep(
  members: readonly { provisional: readonly boolean[]; values: readonly (number | null)[] }[],
  stepCount: number,
): StepValues {
  const values: (number | null)[] = [];
  const provisional: boolean[] = [];

  for (let index = 0; index < stepCount; index += 1) {
    let sum: number | null = null;
    let open = false;
    for (const member of members) {
      const value = member.values[index];
      if (value === null || value === undefined) continue;
      sum = (sum ?? 0) + value;
      open ||= member.provisional[index] ?? false;
    }
    values.push(sum);
    provisional.push(open);
  }

  return { provisional, values };
}

/**
 * The other grid's series re-read on the base grid's steps. Grids come from separate queries, so
 * steps are paired by time; grids of different step widths or store incarnations, or a step the
 * other grid does not cover, are absent.
 */
export function alignedSeries(
  base: MetricGrid,
  other: MetricGrid,
  series: MetricGridSeries | undefined,
): StepValues {
  const comparable =
    base.stepMs === other.stepMs && base.incarnation.id === other.incarnation.id && series;
  const otherIndex = (index: number) => {
    const shifted = (base.startMs - other.startMs) / base.stepMs + index;
    return comparable && Number.isInteger(shifted) ? shifted : -1;
  };

  return {
    provisional: Array.from(
      { length: base.stepCount },
      (_, index) => series?.provisional[otherIndex(index)] ?? false,
    ),
    values: Array.from(
      { length: base.stepCount },
      (_, index) => series?.values[otherIndex(index)] ?? null,
    ),
  };
}

/** The grid's series of one node. */
export function nodeSeries(
  grid: MetricGrid | undefined,
  node: string,
): MetricGridSeries | undefined {
  return grid?.series.find((series) => series.labels[dimensionNames.node] === node);
}

/** The grid's series of one device on one node. */
export function deviceSeries(
  grid: MetricGrid | undefined,
  node: string,
  device: string,
): MetricGridSeries | undefined {
  return grid?.series.find(
    (series) =>
      series.labels[dimensionNames.node] === node &&
      series.labels[dimensionNames.device] === device,
  );
}
