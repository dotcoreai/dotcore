import type { ChartSeries } from "@/components/metrics/time-series-chart/chart-data";
import {
  alignedSeries,
  deviceSeries,
  type HistoryBreakdown,
  nodeSeries,
  sumPerStep,
  usedAgainstCapacity,
} from "@/features/metrics/history-series";
import type { MetricGrid, MetricGridSeries } from "@/features/metrics/metric-grid";
import { dimensionNames, metricNames } from "@/features/metrics/metric-names";
import type { MetricQueryGroup } from "@/features/metrics/use-metric-panel";
import type { MetricHistoryQuery } from "@/features/metrics/use-metric-watch";

import type { DescribedMachine } from "./described-machines";
import type { MemoryKind } from "./site-memory";
import { acceleratorFacts, isDedicated, modelMemoryKind } from "./snapshot-facts";

/** How a model deployment's history line is named and colored, like its bar segment and card. */
export interface ModelLineRef {
  colorKey: string;
  name: string;
}

/** The name of a model line whose deployment was removed before the page saw it. */
export const removedModelName = "Removed model";

/**
 * The RAM and VRAM history queries, grouped by node (and device) with no node filter, so history
 * also covers machines not counted now. Each group ends with the per-deployment memory query when a
 * described machine runs its models in that row; VRAM is read only when a described machine has a
 * dedicated GPU. Nothing is read while no machine is described.
 */
export function memoryQueryGroups(described: readonly DescribedMachine[]): MetricQueryGroup[] {
  if (!described.length) return [];

  const kinds = new Set(described.map((machine) => modelMemoryKind(machine.snapshot)));
  const models = (kind: MemoryKind): MetricHistoryQuery[] =>
    kinds.has(kind)
      ? [
          query(
            metricNames.deploymentAcceleratorMemoryUsed,
            dimensionNames.deployment,
            dimensionNames.node,
            dimensionNames.device,
          ),
        ]
      : [];
  const groups: MetricQueryGroup[] = [
    {
      current: [],
      history: [
        query(metricNames.nodeHostMemoryTotal, dimensionNames.node),
        query(metricNames.nodeHostMemoryAvailable, dimensionNames.node),
        ...models("ram"),
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
        ...models("vram"),
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

/**
 * One line per model deployment whose measured memory sits in the given row: the deployment's
 * devices summed per step, on machines whose latest snapshot runs models in that row. A machine
 * whose GPUs can't be read draws none. Lines are named and colored like the deployment's card, or
 * named as removed when the page never saw it, and ordered by their latest value, largest first.
 * Steps are paired by time with the chart's base grid, which sets the time axis.
 */
export function modelMemoryLines(
  base: MetricGrid,
  grid: MetricGrid,
  described: readonly DescribedMachine[],
  models: ReadonlyMap<string, ModelLineRef>,
  kind: MemoryKind,
): ChartSeries[] {
  const kinds = new Map(
    described.map((machine) => [machine.machineId, modelMemoryKind(machine.snapshot)]),
  );
  const byDeployment = new Map<string, MetricGridSeries[]>();
  for (const series of grid.series) {
    if (kinds.get(series.labels[dimensionNames.node]) !== kind) continue;

    const deployment = series.labels[dimensionNames.deployment];
    byDeployment.set(deployment, [...(byDeployment.get(deployment) ?? []), series]);
  }

  const lines = [...byDeployment].map(([deployment, series]) => {
    const model = models.get(deployment);
    const line: ChartSeries = {
      color: { resource: model?.colorKey ?? deployment },
      id: `model:${deployment}`,
      label: model?.name ?? removedModelName,
      ...sumPerStep(
        series.map((entry) => alignedSeries(base, grid, entry)),
        base.stepCount,
      ),
    };
    return { deployment, latest: latestValue(line.values), line };
  });

  return lines
    .sort(
      (a, b) =>
        (b.latest ?? -Infinity) - (a.latest ?? -Infinity) ||
        a.deployment.localeCompare(b.deployment),
    )
    .map((entry) => entry.line);
}

function query(metric: string, ...groupBy: string[]): MetricHistoryQuery {
  return { groupBy, matchers: {}, metric };
}

function latestValue(values: readonly (number | null)[]): number | undefined {
  for (let index = values.length - 1; index >= 0; index -= 1) {
    const value = values[index];
    if (value !== null) return value;
  }
  return undefined;
}

function deviceKey(node: string, device: string): string {
  return `${node}\n${device}`;
}
