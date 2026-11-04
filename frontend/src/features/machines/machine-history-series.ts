import {
  capacitySeries,
  type ChartColor,
  type ChartSeries,
} from "@/components/metrics/time-series-chart/chart-data";
import { alignedSeries } from "@/features/metrics/history-series";
import type { MetricGrid } from "@/features/metrics/metric-grid";

/**
 * Used host memory: total minus available at each step, never negative, with the total as the
 * capacity line. Reclaimable cache counts as available, so it is never drawn as used. The two grids
 * come from separate queries, so steps are paired by time rather than position; a step missing
 * either reading, or grids from different step widths or store incarnations, is absent, not zero.
 */
export function hostMemorySeries(total: MetricGrid, available: MetricGrid): ChartSeries[] {
  const totalSeries = total.series[0];
  const availableOnTotal = alignedSeries(total, available, available.series[0]);
  const values = Array.from({ length: total.stepCount }, (_, index) => {
    const totalValue = totalSeries?.values[index] ?? null;
    const availableValue = availableOnTotal.values[index];
    return totalValue === null || availableValue === null
      ? null
      : Math.max(0, totalValue - availableValue);
  });
  const provisional = Array.from(
    { length: total.stepCount },
    (_, index) => totalSeries?.provisional[index] || availableOnTotal.provisional[index] || false,
  );

  return [
    usedSeries(values, provisional),
    capacitySeries(
      totalSeries?.values ?? absentValues(total.stepCount),
      total.stepCount,
      totalSeries?.provisional,
    ),
  ];
}

/**
 * Used memory of one accelerator device against its total, which comes from the hardware
 * snapshot; absent steps where the device reported nothing.
 */
export function deviceMemorySeries(
  deviceId: string,
  used: MetricGrid,
  totalBytes: number,
): ChartSeries[] {
  const series = used.series.find((entry) => entry.labels.device === deviceId);

  return [
    usedSeries(series?.values ?? absentValues(used.stepCount), series?.provisional),
    capacitySeries(totalBytes, used.stepCount),
  ];
}

/** Host CPU utilization of the machine as a ratio; absent steps where the host reported nothing. */
export function hostCpuSeries(utilization: MetricGrid): ChartSeries[] {
  const series = utilization.series[0];

  return [
    {
      color: "chart-1",
      id: "cpu",
      label: "CPU",
      provisional: series?.provisional,
      values: series?.values ?? absentValues(utilization.stepCount),
    },
  ];
}

/**
 * One device's line of a per-device signal, read from the grid's series labeled with that device.
 * A device that reported nothing keeps its line, with every step absent.
 */
export function deviceSignalSeries(
  deviceId: string,
  grid: MetricGrid,
  label: string,
  color: ChartColor,
): ChartSeries {
  const series = grid.series.find((entry) => entry.labels.device === deviceId);

  return {
    color,
    id: deviceId,
    label,
    provisional: series?.provisional,
    values: series?.values ?? absentValues(grid.stepCount),
  };
}

/**
 * Used space of one filesystem against its size, drawn as the capacity line. Used steps
 * lead; the size is paired to them by time, and absent where either grid has no evidence.
 */
export function filesystemSeries(
  filesystem: { fstype: string; mountpoint: string },
  used: MetricGrid,
  size: MetricGrid,
): ChartSeries[] {
  const matches = (entry: { labels: Record<string, string> }) =>
    entry.labels.mountpoint === filesystem.mountpoint && entry.labels.fstype === filesystem.fstype;
  const usedLine = used.series.find(matches);
  const sizeSeries = size.series.find(matches);
  const sizeOnUsed = alignedSeries(used, size, sizeSeries);

  return [
    {
      color: "chart-1",
      id: "used",
      label: "Used",
      provisional: usedLine?.provisional,
      values: usedLine?.values ?? absentValues(used.stepCount),
    },
    capacitySeries(sizeOnUsed.values, used.stepCount, sizeOnUsed.provisional),
  ];
}

function absentValues(stepCount: number): null[] {
  return Array.from({ length: stepCount }, () => null);
}

function usedSeries(
  values: (number | null)[],
  provisional: readonly boolean[] | undefined,
): ChartSeries {
  return { color: "chart-1", id: "used", label: "Used", provisional, values };
}
