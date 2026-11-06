import { useMemo } from "react";

import type { MetricsClient } from "@/api/metrics-client";
import { resourcesCurrentWindow } from "@/features/machines/machine-resources";
import { currentValue } from "@/features/metrics/metric-current";
import type { MetricGrid } from "@/features/metrics/metric-grid";
import { dimensionNames, metricNames } from "@/features/metrics/metric-names";
import { useMetricsPausedByLogs } from "@/features/metrics/metric-pause";
import type { MetricRange } from "@/features/metrics/metric-ranges";
import { type MetricHistoryQuery, useMetricAnswers } from "@/features/metrics/use-metric-watch";

/** What an application deployment currently uses of its machine; a value without a reading is absent. */
export interface ApplicationResources {
  /** Share of the hosting machine's CPUs, as a ratio. */
  cpu?: number;
  /** Host memory used, in bytes. */
  memoryBytes?: number;
}

/**
 * The current CPU and memory use of the application deployments, by deployment id, watched live in
 * one small stream of the application gauges with the services of a deployment summed. The stream
 * opens only while `enabled`, such as when deployments are listed.
 */
export function useApplicationResources(
  client: MetricsClient,
  enabled: boolean,
): ReadonlyMap<string, ApplicationResources> {
  const paused = useMetricsPausedByLogs();
  const watch = useMetricAnswers(
    client,
    enabled ? resourceQueries : [],
    enabled ? resourcesWindow : undefined,
    { paused },
  );
  const cpu = watch?.value?.[0]?.grid;
  const memory = watch?.value?.[1]?.grid;

  return useMemo(() => applicationResources(cpu, memory), [cpu, memory]);
}

/**
 * Each deployment's current use from the CPU and memory grids grouped by deployment. A deployment
 * with no recent reading in either is absent.
 */
export function applicationResources(
  cpu: MetricGrid | undefined,
  memory: MetricGrid | undefined,
): Map<string, ApplicationResources> {
  const resources = new Map<string, ApplicationResources>();
  const add = (grid: MetricGrid | undefined, key: keyof ApplicationResources) => {
    for (const series of grid?.series ?? []) {
      const id = series.labels[dimensionNames.deployment];
      const value = currentValue(series, freshSteps, { includeOpen: true });
      if (id === undefined || value === undefined) continue;

      resources.set(id, { ...resources.get(id), [key]: value });
    }
  };
  add(cpu, "cpu");
  add(memory, "memoryBytes");

  return resources;
}

// Apps report every second; a reading a few seconds old no longer counts as current.
const freshSteps = 5;

const resourceQueries: readonly MetricHistoryQuery[] = [
  {
    groupBy: [dimensionNames.deployment],
    matchers: {},
    metric: metricNames.applicationHostCpuUtilization,
  },
  {
    groupBy: [dimensionNames.deployment],
    matchers: {},
    metric: metricNames.applicationHostMemoryUsed,
  },
];

const resourcesWindow: MetricRange = { ...resourcesCurrentWindow, label: "now" };
