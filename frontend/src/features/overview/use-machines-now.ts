import { useMemo } from "react";

import type { MetricsClient } from "@/api/metrics-client";
import { useMetricsPausedByLogs } from "@/features/metrics/metric-pause";
import type { MetricRange } from "@/features/metrics/metric-ranges";
import { useMetricAnswers } from "@/features/metrics/use-metric-watch";

import { type MachineFacts, type MachineNow, machineNow } from "./machine-site-values";
import { machineMetrics, machineQueries } from "./overview-machine-specs";

/** The newest closed readings of the site and of each machine. */
export interface MachinesNow {
  machines: ReadonlyMap<string, MachineNow>;
  site: MachineNow;
}

/**
 * The machines' readings of the newest closed one-second step, from a small stream of their own
 * next to the page's, so the headline values neither flicker with an open bucket nor wait for the
 * page range's coarse steps. Absent until the first answer; a stream paused by a log viewer keeps
 * what it read.
 */
export function useMachinesNow(
  client: MetricsClient,
  facts: MachineFacts,
): MachinesNow | undefined {
  const paused = useMetricsPausedByLogs();
  const watch = useMetricAnswers(client, nowQueries, nowRange, { paused });
  const answers = watch?.value;

  return useMemo(
    () =>
      answers
        ? machineNow(
            Object.fromEntries(
              machineMetrics.map((metric, index) => [metric, answers[index]?.stats]),
            ),
            facts,
          )
        : undefined,
    [answers, facts],
  );
}

/**
 * The window of the "now" stream: a few one-second steps, so the newest closed step trails the
 * clock by a second or two and an open step never shows.
 */
const nowRange: MetricRange = { durationMs: 10_000, label: "now", stepMs: 1000 };

const nowQueries = machineMetrics.map((metric) => machineQueries[metric]);
