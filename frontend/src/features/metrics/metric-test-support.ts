import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { EmptySchema } from "@bufbuild/protobuf/wkt";

import type { MetricsClient } from "@/api/metrics-client";
import {
  MetricCounterValueSchema,
  MetricDistributionValueSchema,
  type MetricPoint,
  MetricPointSchema,
  type MetricQueryResult,
  MetricQueryResultSchema,
  type MetricQuerySchema,
  type MetricSeries,
  MetricSeriesSchema,
} from "@/gen/dotcore/metrics/v1/metric_pb";
import {
  ListMetricDescriptorsResponseSchema,
  MetricResolutionSchema,
  WatchMetricsResponseSchema,
} from "@/gen/dotcore/metrics/v1/service_pb";

/** Epoch second of the grid start used by metric tests. */
export const gridStartSeconds = 1_800_000_000;

/** A gauge point `offset` seconds after the grid start. */
export function gaugePoint(offset: number, value: number, provisional = false): MetricPoint {
  return create(MetricPointSchema, {
    bucketStart: { seconds: BigInt(gridStartSeconds + offset) },
    provisional,
    value: { case: "gauge", value },
  });
}

/** A counter point whose delta defaults to its rate, as at a one-second step. */
export function counterPoint(
  offset: number,
  ratePerSecond: number,
  provisional = false,
  delta = ratePerSecond,
): MetricPoint {
  return create(MetricPointSchema, {
    bucketStart: { seconds: BigInt(gridStartSeconds + offset) },
    provisional,
    value: {
      case: "counter",
      value: create(MetricCounterValueSchema, { delta, ratePerSecond }),
    },
  });
}

/**
 * A distribution point whose percentile estimates are given as quantile to value, carrying the
 * sample `count` and `sum` of its bucket.
 */
export function distributionPoint(
  offset: number,
  percentiles: Record<number, number>,
  provisional = false,
  { count = 1, sum = 0 }: { count?: number; sum?: number } = {},
): MetricPoint {
  return create(MetricPointSchema, {
    bucketStart: { seconds: BigInt(gridStartSeconds + offset) },
    provisional,
    value: {
      case: "distribution",
      value: create(MetricDistributionValueSchema, {
        count: BigInt(count),
        percentiles: Object.entries(percentiles).map(([quantile, value]) => ({
          quantile: Number(quantile),
          value,
        })),
        sum,
      }),
    },
  });
}

export function metricSeries(labels: Record<string, string>, points: MetricPoint[]): MetricSeries {
  return create(MetricSeriesSchema, { labels, points });
}

/** An answer covering `stepCount` steps of `stepSeconds` from the grid start. */
export function queryResult({
  incarnationId = "incarnation-a",
  series,
  startedAtOffset = 0,
  startOffset = 0,
  stepCount,
  stepSeconds = 1,
}: {
  incarnationId?: string;
  series: MetricSeries[];
  startedAtOffset?: number;
  /** Seconds the covered range starts after the grid start. */
  startOffset?: number;
  stepCount: number;
  stepSeconds?: number;
}): MetricQueryResult {
  return create(MetricQueryResultSchema, {
    coveredRange: {
      end: { seconds: BigInt(gridStartSeconds + startOffset + stepCount * stepSeconds) },
      start: { seconds: BigInt(gridStartSeconds + startOffset) },
    },
    effectiveStep: { seconds: BigInt(stepSeconds) },
    incarnation: {
      id: incarnationId,
      startedAt: { seconds: BigInt(gridStartSeconds + startedAtOffset) },
    },
    series,
  });
}

/**
 * A fake watch that answers each keyed query of a request with one upsert, then a single
 * `backfill_complete`, and stays open until the caller aborts it.
 */
export function staticWatch(
  answer: (query: MessageInitShape<typeof MetricQuerySchema> | undefined) => MetricQueryResult,
): MetricsClient["metrics"]["watchMetrics"] {
  return ({ queries }, options) =>
    (async function* () {
      for (const { key, query } of queries ?? []) {
        yield create(WatchMetricsResponseSchema, {
          event: { case: "upsert", value: { key, result: answer(query) } },
        });
      }
      yield create(WatchMetricsResponseSchema, {
        event: { case: "backfillComplete", value: create(EmptySchema) },
      });
      await new Promise<void>((resolve) => {
        if (options?.signal?.aborted) resolve();
        options?.signal?.addEventListener("abort", () => resolve());
      });
    })();
}

/**
 * A metrics client whose store retains 15 minutes at one-second steps and 6 hours at ten-second
 * steps, and watches as given.
 */
export function metricsClientWith(
  watchMetrics: MetricsClient["metrics"]["watchMetrics"],
): MetricsClient {
  return {
    metrics: {
      listMetricDescriptors: () =>
        Promise.resolve(
          create(ListMetricDescriptorsResponseSchema, {
            resolutions: [
              create(MetricResolutionSchema, {
                retention: { seconds: 900n },
                step: { seconds: 1n },
              }),
              create(MetricResolutionSchema, {
                retention: { seconds: 21_600n },
                step: { seconds: 10n },
              }),
            ],
          }),
        ),
      watchMetrics,
    },
  };
}

/** A metrics client that never answers, for tests that don't read metrics. */
export function idleMetricsClient(): MetricsClient {
  return {
    metrics: {
      listMetricDescriptors: () => new Promise(() => undefined),
      watchMetrics: (_, options) =>
        (async function* () {
          await new Promise<void>((resolve) => {
            if (options?.signal?.aborted) resolve();
            options?.signal?.addEventListener("abort", () => resolve());
          });
          yield* [];
        })(),
    },
  };
}
