import type { MetricStatsGrid } from "@/features/metrics/metric-grid";

import type { MachineFacts } from "./machine-site-values";

/** One test metric series: its labels, step values and the steps still open. */
export interface TestSeries {
  labels: Record<string, string>;
  open?: number[];
  values: (number | null)[];
}

/** A metric stats grid over the given series, one gauge reading per step. */
export function grid(...series: TestSeries[]): MetricStatsGrid {
  const stepCount = series[0]?.values.length ?? 0;

  return {
    incarnation: { id: "a", startedAtMs: 0 },
    series: series.map((entry) => ({
      labels: entry.labels,
      provisional: entry.values.map((_, index) => entry.open?.includes(index) ?? false),
      steps: entry.values.map((value) => (value === null ? null : { kind: "gauge", value })),
    })),
    startMs: 0,
    stepCount,
    stepMs: 1000,
  };
}

/** A per-machine series. */
export const node = (id: string, values: (number | null)[], open?: number[]): TestSeries => ({
  labels: { node: id },
  open,
  values,
});

/** A per-GPU series of one machine. */
export const device = (id: string, deviceId: string, values: (number | null)[]): TestSeries => ({
  labels: { device: deviceId, node: id },
  values,
});

/** Two machines: a big one with two GPUs, one of them dedicated, and a small one with one. */
export const facts: MachineFacts = {
  cpus: new Map([
    ["big", 48],
    ["small", 16],
  ]),
  dedicated: new Set(["big/gpu0"]),
  gpus: new Map([
    ["big", 2],
    ["small", 1],
  ]),
};
