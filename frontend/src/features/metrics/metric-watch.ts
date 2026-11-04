import { create } from "@bufbuild/protobuf";

import {
  type MetricPoint,
  type MetricQueryResult,
  MetricQueryResultSchema,
  MetricSeriesSchema,
} from "@/gen/dotcore/metrics/v1/metric_pb";

import {
  durationMs,
  fillMetricGrid,
  fillMetricStatsGrid,
  type MetricGrid,
  type MetricStatsGrid,
  timestampMs,
} from "./metric-grid";

/**
 * The points a metric watch has delivered so far, merged by series labels and bucket start. An
 * upsert replaces only the points it carries (a reopened watch resends its full range, which merges over the held points), points before the covered range are dropped as it
 * slides, and a new store incarnation or effective step replaces everything held. Each grid view
 * is built once per change, so reading it again without an upsert returns the same object.
 */
export class MetricWatchAccumulator {
  private cache: { grid?: MetricGrid; statsGrid?: MetricStatsGrid } = {};
  private latest: MetricQueryResult | undefined;
  private series = new Map<string, HeldSeries>();

  apply(upsert: MetricQueryResult): void {
    this.cache = {};
    const incarnationId = upsert.incarnation?.id ?? "";
    const stepMs = durationMs(upsert.effectiveStep);
    if (
      this.latest &&
      (incarnationId !== (this.latest.incarnation?.id ?? "") ||
        stepMs !== durationMs(this.latest.effectiveStep))
    ) {
      this.series.clear();
    }
    this.latest = upsert;

    const startMs = timestampMs(upsert.coveredRange?.start);
    for (const incoming of upsert.series) {
      const key = labelsKey(incoming.labels);
      const held = this.series.get(key) ?? { labels: incoming.labels, points: new Map() };
      this.series.set(key, held);

      for (const point of incoming.points) {
        const bucketMs = timestampMs(point.bucketStart);
        if (bucketMs < startMs) continue;

        held.points.set(bucketMs, point);
      }
    }

    // Emptied groups stay, as a query lists every matching group even without points.
    for (const held of this.series.values()) {
      for (const bucketMs of held.points.keys()) {
        if (bucketMs < startMs) held.points.delete(bucketMs);
      }
    }
  }

  /** The held points on the grid of the latest covered range, as per-second rates and values. */
  grid(): MetricGrid | undefined {
    if (this.cache.grid) return this.cache.grid;
    const result = this.heldResult();
    if (!result) return undefined;

    return (this.cache.grid ??= fillMetricGrid(result));
  }

  /** The held points on the same grid, keeping gauge values, counter deltas and distribution sums. */
  statsGrid(): MetricStatsGrid | undefined {
    if (this.cache.statsGrid) return this.cache.statsGrid;
    const result = this.heldResult();
    if (!result) return undefined;

    return (this.cache.statsGrid ??= fillMetricStatsGrid(result));
  }

  private heldResult(): MetricQueryResult | undefined {
    if (!this.latest) return undefined;

    return create(MetricQueryResultSchema, {
      coveredRange: this.latest.coveredRange,
      effectiveStep: this.latest.effectiveStep,
      incarnation: this.latest.incarnation,
      series: [...this.series.values()].map((held) =>
        create(MetricSeriesSchema, {
          labels: held.labels,
          points: [...held.points.values()],
        }),
      ),
    });
  }
}

interface HeldSeries {
  labels: Record<string, string>;
  points: Map<number, MetricPoint>;
}

function labelsKey(labels: Record<string, string>): string {
  return JSON.stringify(Object.entries(labels).sort(([a], [b]) => a.localeCompare(b)));
}
