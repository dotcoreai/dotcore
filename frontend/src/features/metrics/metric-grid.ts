import type { Duration, Timestamp } from "@bufbuild/protobuf/wkt";

import type {
  MetricPoint,
  MetricQueryResult,
  MetricSeries,
} from "@/gen/dotcore/metrics/v1/metric_pb";

/** One series on the regular grid: a value per step, `null` where the store has no evidence. */
export interface MetricGridSeries {
  labels: Readonly<Record<string, string>>;
  /** Whether the step's bucket is still open and may change. */
  provisional: boolean[];
  /** The distribution quantile (0 to 1) this series carries; absent for gauges and counters. */
  quantile?: number;
  values: (number | null)[];
}

/** A query answer laid out on its effective-step grid, ready to chart. */
export interface MetricGrid {
  /** When the store lifetime this answer comes from began; no history exists before it. */
  incarnation: { id: string; startedAtMs: number };
  series: MetricGridSeries[];
  startMs: number;
  stepMs: number;
  stepCount: number;
}

/**
 * Fills the step grid of a query answer. The store sends only buckets that have evidence, so
 * every other step becomes `null`: an absent point is never turned into zero. Points outside the
 * covered range, and non-finite values, are ignored. A counter point yields its per-second rate. A
 * distribution series becomes one grid series per requested quantile, in request order.
 */
export function fillMetricGrid(result: MetricQueryResult): MetricGrid {
  const layout = gridLayout(result);

  return {
    ...layout,
    series: result.series.flatMap((series) =>
      fillSeries(series, layout.startMs, layout.stepMs, layout.stepCount),
    ),
  };
}

/** What one step of a series holds, by metric kind. */
export type MetricStepStats =
  | { kind: "counter"; delta: number }
  | { count: number; kind: "distribution"; sum: number }
  | { kind: "gauge"; value: number };

/** One series on the regular grid, keeping the raw per-step stats instead of display values. */
export interface MetricStatsSeries {
  labels: Readonly<Record<string, string>>;
  /** Whether the step's bucket is still open and may change. */
  provisional: boolean[];
  /** The step's stats; `null` where the store has no evidence. */
  steps: (MetricStepStats | null)[];
}

/** A query answer laid out on its effective-step grid with per-step stats rather than rates. */
export interface MetricStatsGrid {
  incarnation: { id: string; startedAtMs: number };
  series: MetricStatsSeries[];
  startMs: number;
  stepCount: number;
  stepMs: number;
}

/**
 * Lays a query answer on its step grid like `fillMetricGrid`, but keeps what range totals and
 * means need: the gauge value, the counter delta, and the distribution sample count and sum.
 * Absent steps and non-finite stats are `null`, never zero.
 */
export function fillMetricStatsGrid(result: MetricQueryResult): MetricStatsGrid {
  const layout = gridLayout(result);

  return {
    incarnation: layout.incarnation,
    series: result.series.map((series) => {
      const steps = Array.from({ length: layout.stepCount }, (): MetricStepStats | null => null);
      const provisional = Array.from({ length: layout.stepCount }, () => false);

      for (const point of series.points) {
        const index = stepIndex(point, layout);
        const stats = pointStats(point);
        if (index < 0 || index >= layout.stepCount || stats === null) continue;

        steps[index] = stats;
        provisional[index] = point.provisional;
      }

      return { labels: series.labels, provisional, steps };
    }),
    startMs: layout.startMs,
    stepCount: layout.stepCount,
    stepMs: layout.stepMs,
  };
}

export function durationMs(duration: Duration | undefined): number {
  return duration ? Number(duration.seconds) * 1000 + duration.nanos / 1e6 : 0;
}

export function timestampMs(timestamp: Timestamp | undefined): number {
  return timestamp ? Number(timestamp.seconds) * 1000 + timestamp.nanos / 1e6 : 0;
}

type GridLayout = Pick<MetricGrid, "incarnation" | "startMs" | "stepCount" | "stepMs">;

function gridLayout(result: MetricQueryResult): GridLayout {
  const stepMs = durationMs(result.effectiveStep);
  const startMs = timestampMs(result.coveredRange?.start);
  const endMs = timestampMs(result.coveredRange?.end);
  const stepCount = stepMs > 0 ? Math.max(0, Math.round((endMs - startMs) / stepMs)) : 0;

  return {
    incarnation: {
      id: result.incarnation?.id ?? "",
      startedAtMs: timestampMs(result.incarnation?.startedAt),
    },
    startMs,
    stepCount,
    stepMs,
  };
}

function stepIndex(point: MetricPoint, layout: GridLayout): number {
  return Math.round((timestampMs(point.bucketStart) - layout.startMs) / layout.stepMs);
}

function pointStats(point: MetricPoint): MetricStepStats | null {
  switch (point.value.case) {
    case "gauge":
      return Number.isFinite(point.value.value)
        ? { kind: "gauge", value: point.value.value }
        : null;
    case "counter":
      return Number.isFinite(point.value.value.delta)
        ? { delta: point.value.value.delta, kind: "counter" }
        : null;
    case "distribution": {
      const { count, sum } = point.value.value;
      return Number.isFinite(sum) ? { count: Number(count), kind: "distribution", sum } : null;
    }
    default:
      return null;
  }
}

function fillSeries(
  series: MetricSeries,
  startMs: number,
  stepMs: number,
  stepCount: number,
): MetricGridSeries[] {
  const quantiles = distributionQuantiles(series);
  const columns = quantiles.length ? quantiles : [undefined];

  return columns.map((quantile) => {
    const values = Array.from({ length: stepCount }, (): number | null => null);
    const provisional = Array.from({ length: stepCount }, () => false);

    for (const point of series.points) {
      const index = Math.round((timestampMs(point.bucketStart) - startMs) / stepMs);
      const value = pointValue(point, quantile);
      if (index < 0 || index >= stepCount || value === null) continue;

      values[index] = value;
      provisional[index] = point.provisional;
    }

    return {
      labels: series.labels,
      provisional,
      ...(quantile === undefined ? {} : { quantile }),
      values,
    };
  });
}

/** The quantiles estimated by the series' distribution points, in the order first seen. */
function distributionQuantiles(series: MetricSeries): number[] {
  const quantiles: number[] = [];

  for (const point of series.points) {
    if (point.value.case !== "distribution") continue;
    for (const percentile of point.value.value.percentiles) {
      if (!quantiles.includes(percentile.quantile)) quantiles.push(percentile.quantile);
    }
  }

  return quantiles;
}

function pointValue(point: MetricPoint, quantile: number | undefined): number | null {
  const value = rawPointValue(point, quantile);
  return value !== null && Number.isFinite(value) ? value : null;
}

function rawPointValue(point: MetricPoint, quantile: number | undefined): number | null {
  switch (point.value.case) {
    case "gauge":
      return point.value.value;
    case "counter":
      return point.value.value.ratePerSecond;
    case "distribution":
      return (
        point.value.value.percentiles.find((percentile) => percentile.quantile === quantile)
          ?.value ?? null
      );
    default:
      return null;
  }
}
