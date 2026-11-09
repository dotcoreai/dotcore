import {
  type ChartUnit,
  formatChartValue,
  formatChartValueParts,
} from "@/components/metrics/time-series-chart/chart-data";
import { dimensionNames, metricNames } from "@/features/metrics/metric-names";
import type { MetricHistoryQuery } from "@/features/metrics/use-metric-watch";
import { MetricAggregate } from "@/gen/dotcore/metrics/v1/metric_pb";

import type { MachineMetric } from "./machine-site-values";

/**
 * The machine metrics the console reads, each per machine (and per GPU for memory), in the order
 * the "now" stream asks for them. GPU utilization is averaged over a machine's GPUs, temperature is
 * its hottest GPU, storage its largest filesystem; every other reading is summed. A bucket reduces
 * to its latest reading unless a reader asks otherwise.
 */
export const machineMetrics: readonly MachineMetric[] = [
  "cpu",
  "gpu",
  "power",
  "temperature",
  "ramTotal",
  "ramAvailable",
  "vramUsed",
  "vramTotal",
  "storageSize",
  "storageUsed",
];

/** The metric query that reads each machine metric. */
export const machineQueries: Readonly<Record<MachineMetric, MetricHistoryQuery>> = (() => {
  const query = (
    metric: string,
    groupBy: readonly string[],
    aggregate = MetricAggregate.SUM,
  ): MetricHistoryQuery => ({ aggregate, groupBy, matchers: {}, metric });
  const node = [dimensionNames.node];
  const device = [dimensionNames.node, dimensionNames.device];

  return {
    cpu: query(metricNames.nodeHostCpuUtilization, node, MetricAggregate.AVG),
    gpu: query(metricNames.nodeAcceleratorUtilization, node, MetricAggregate.AVG),
    power: query(metricNames.nodeAcceleratorPower, node),
    ramAvailable: query(metricNames.nodeHostMemoryAvailable, node),
    ramTotal: query(metricNames.nodeHostMemoryTotal, node),
    storageSize: query(metricNames.nodeFilesystemSize, node, MetricAggregate.MAX),
    storageUsed: query(metricNames.nodeFilesystemUsed, node, MetricAggregate.MAX),
    temperature: query(metricNames.nodeAcceleratorTemperature, node, MetricAggregate.MAX),
    vramTotal: query(metricNames.nodeAcceleratorMemoryTotal, device),
    vramUsed: query(metricNames.nodeAcceleratorMemoryUsed, device),
  };
})();

/** A reading in its unit, such as "45%" or "150 W"; "—" when absent. */
export function formatReading(unit: ChartUnit, value: number | undefined): string {
  return value === undefined ? "—" : formatChartValue(unit, value);
}

/** Used against total memory as "12.3 / 64 GiB" over one unit, or in full when the units differ. */
export function formatUsed(used: number | undefined, total: number | undefined): string {
  const { unit, value } = usedParts(used, total);
  return unit ? `${value} ${unit}` : value;
}

/**
 * Used against total memory split into value and unit, such as "12.3 / 64" and "GiB", or in full
 * with no unit when the two units differ; "—" when either is absent.
 */
export function usedParts(
  used: number | undefined,
  total: number | undefined,
): { unit?: string; value: string } {
  if (used === undefined || total === undefined) return { value: "—" };

  const usedParts = formatChartValueParts("bytes", used);
  const totalParts = formatChartValueParts("bytes", total);

  return usedParts.unit === totalParts.unit
    ? { unit: totalParts.unit, value: `${usedParts.value} / ${totalParts.value}` }
    : { value: `${formatChartValue("bytes", used)} / ${formatChartValue("bytes", total)}` };
}
