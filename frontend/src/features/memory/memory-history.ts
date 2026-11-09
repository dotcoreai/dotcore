import type { ChartSeries } from "@/components/metrics/time-series-chart/chart-data";
import {
  alignedSeries,
  deviceSeries,
  type HistoryBreakdown,
  nodeSeries,
  usedAgainstCapacity,
} from "@/features/metrics/history-series";
import type { MetricGrid } from "@/features/metrics/metric-grid";
import { dimensionNames, metricNames } from "@/features/metrics/metric-names";
import type { MetricQueryGroup } from "@/features/metrics/use-metric-panel";
import type { MetricHistoryQuery } from "@/features/metrics/use-metric-watch";

import type { DescribedMachine } from "./described-machines";
import { acceleratorFacts, isDedicated } from "./snapshot-facts";

/**
 * The RAM and VRAM history queries, grouped by node (and device) with no node filter, so history
 * also covers machines not counted now. VRAM is read only when a described machine has a dedicated
 * GPU. Nothing is read while no machine is described.
 */
export function memoryQueryGroups(described: readonly DescribedMachine[]): MetricQueryGroup[] {
  if (!described.length) return [];

  const groups: MetricQueryGroup[] = [
    {
      current: [],
      history: [
        query(metricNames.nodeHostMemoryTotal, dimensionNames.node),
        query(metricNames.nodeHostMemoryAvailable, dimensionNames.node),
      ],
      id: "ram",
    },
  ];

  if (described.some((machine) => acceleratorFacts(machine.snapshot).some(isDedicated))) {
    groups.push({
      current: [],
      history: [
        query(metricNames.nodeAcceleratorMemoryUsed, dimensionNames.node, dimensionNames.device),
        query(metricNames.nodeAcceleratorMemoryTotal, dimensionNames.node, dimensionNames.device),
      ],
      id: "vram",
    });
  }

  return groups;
}

/**
 * Used host memory history: total minus available per machine. Combined, each step sums used and
 * total over the machines reporting both in it, so the used line and its capacity line always
 * cover the same machines.
 */
export function ramHistory(
  total: MetricGrid,
  available: MetricGrid,
  names: ReadonlyMap<string, string>,
  breakdown: HistoryBreakdown,
): ChartSeries[] {
  const members = total.series.map((totalSeries) => {
    const node = totalSeries.labels[dimensionNames.node];
    const availableOnTotal = alignedSeries(total, available, nodeSeries(available, node));
    return {
      capacity: totalSeries,
      key: node,
      used: {
        provisional: totalSeries.values.map(
          (_, index) => totalSeries.provisional[index] || availableOnTotal.provisional[index],
        ),
        values: totalSeries.values.map((value, index) => {
          const free = availableOnTotal.values[index];
          return value === null || free === null ? null : Math.max(0, value - free);
        }),
      },
    };
  });

  return usedAgainstCapacity(members, total.stepCount, names, breakdown);
}

/**
 * Dedicated GPU memory history. Only devices the described machines' latest snapshots name as
 * dedicated are drawn, since a unified GPU's memory is the host's. Combined, each step pairs used
 * and total over the devices reporting both.
 */
export function vramHistory(
  used: MetricGrid,
  total: MetricGrid,
  described: readonly DescribedMachine[],
  names: ReadonlyMap<string, string>,
  breakdown: HistoryBreakdown,
): ChartSeries[] {
  const dedicated = new Set(
    described.flatMap((machine) =>
      acceleratorFacts(machine.snapshot)
        .filter(isDedicated)
        .map((device) => deviceKey(machine.machineId, device.deviceId)),
    ),
  );
  const members = used.series.flatMap((usedSeries) => {
    const node = usedSeries.labels[dimensionNames.node];
    const device = usedSeries.labels[dimensionNames.device];
    if (!dedicated.has(deviceKey(node, device))) return [];

    const totalOnUsed = alignedSeries(used, total, deviceSeries(total, node, device));
    return [{ capacity: totalOnUsed, key: node, used: usedSeries }];
  });

  return usedAgainstCapacity(members, used.stepCount, names, breakdown);
}

function query(metric: string, ...groupBy: string[]): MetricHistoryQuery {
  return { groupBy, matchers: {}, metric };
}

function deviceKey(node: string, device: string): string {
  return `${node}\n${device}`;
}
