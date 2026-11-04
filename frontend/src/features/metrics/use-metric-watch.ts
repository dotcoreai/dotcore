import { create } from "@bufbuild/protobuf";
import { DurationSchema } from "@bufbuild/protobuf/wkt";
import { useEffect, useState } from "react";

import type { MetricsClient } from "@/api/metrics-client";
import {
  MetricAggregate,
  type MetricQuery,
  MetricQuerySchema,
  MetricReduce,
} from "@/gen/dotcore/metrics/v1/metric_pb";
import {
  KeyedMetricQuerySchema,
  type WatchMetricsResponse,
} from "@/gen/dotcore/metrics/v1/service_pb";
import type { ReadState } from "@/lib/read-state";
import { runStreamLoop } from "@/lib/stream-loop";

import { metricDescriptorsFor } from "./metric-descriptors";
import type { MetricGrid, MetricStatsGrid } from "./metric-grid";
import type { MetricRange } from "./metric-ranges";
import { MetricWatchAccumulator } from "./metric-watch";

/** One series set to chart: the metric, the labels it is filtered on, and what stays apart. */
export interface MetricHistoryQuery {
  /** How series that share the group-by labels combine; defaults to `SUM`. */
  aggregate?: MetricAggregate;
  groupBy: readonly string[];
  matchers: Readonly<Record<string, string>>;
  metric: string;
  /** Quantiles (0 to 1) estimated for a distribution metric; each becomes its own grid series. */
  percentiles?: readonly number[];
  /** How readings within one bucket reduce; defaults to the latest. Counters need `SUM`. */
  reduce?: MetricReduce;
}

/**
 * Watches the given queries over the last `range` while mounted, at the range's step. All queries
 * share one stream, keyed by their index; the grids appear once the stream has delivered every
 * query's backfill and then follow it, so charts advance as buckets close or are revised. A dropped stream reopens
 * with backoff, resuming from the last bucket received, and is paused while the document is
 * hidden. A failed or reconnecting stream keeps the last grids and reports `failed`. Each query
 * reads the latest gauge reading per bucket unless the query says otherwise (counters give their
 * rate, distributions their percentiles) and combines the series that share its group-by labels
 * by the query's aggregate (the sum by default). Upserts that arrive together publish once.
 * A rejected query (invalid or unknown metric) reports `failed` without reopening. Passing no
 * queries stops watching and forgets the grids read; pausing closes the stream but keeps the last
 * grids in view until it resumes and answers again.
 */
export function useMetricWatch(
  client: MetricsClient,
  queries: readonly MetricHistoryQuery[],
  range: MetricRange | undefined,
  options: { paused?: boolean } = {},
): ReadState<MetricGrid[]> | undefined {
  return useAccumulatorWatch(client, queries, range, selectGrids, options);
}

/** One query's answer, laid out both for charting and for range totals and means. */
export interface MetricAnswer {
  grid: MetricGrid | undefined;
  stats: MetricStatsGrid | undefined;
}

/**
 * Watches like `useMetricWatch`, but answers every query at its own index with both its chart
 * grid and its stats grid, so range reducers and charts read the same stream. A grid stays the
 * same object until its query receives an upsert.
 */
export function useMetricAnswers(
  client: MetricsClient,
  queries: readonly MetricHistoryQuery[],
  range: MetricRange | undefined,
  options: { paused?: boolean } = {},
): ReadState<MetricAnswer[]> | undefined {
  return useAccumulatorWatch(client, queries, range, selectAnswers, options);
}

/** The watch behind both hooks; `select` reads the published value off the accumulators. */
function useAccumulatorWatch<T>(
  client: MetricsClient,
  queries: readonly MetricHistoryQuery[],
  range: MetricRange | undefined,
  select: (accumulators: readonly MetricWatchAccumulator[]) => T,
  { paused = false }: { paused?: boolean },
): ReadState<T> | undefined {
  const queriesKey = JSON.stringify(queries);
  const durationMs = range?.durationMs;
  const stepMs = range?.stepMs;
  const watching = durationMs !== undefined && stepMs !== undefined && queries.length > 0;
  const active = watching && !paused;
  const identity = `${queriesKey}|${durationMs}|${stepMs}`;
  const [state, setState] = useState<{ identity: string; value: ReadState<T> }>();
  const [kept, setKept] = useState<{ queriesKey: string; value: T }>();

  useEffect(() => {
    if (!active) return;

    const descriptors = metricDescriptorsFor(client);
    const watched = JSON.parse(queriesKey) as MetricHistoryQuery[];
    const accumulators = watched.map(() => new MetricWatchAccumulator());
    let ready = false;
    let failed = false;
    let pending: ReturnType<typeof setTimeout> | undefined;
    const publish = () => {
      clearTimeout(pending);
      pending = undefined;
      const value = ready ? select(accumulators) : undefined;
      setState({ identity, value: { failed, value } });
    };

    const stop = watchQueries(
      client,
      watched.map((query) => toMetricQuery(query, durationMs, stepMs)),
      accumulators,
      {
        onBackfillComplete: () => {
          ready = true;
          publish();
        },
        onFailure: () => {
          failed = true;
          publish();
        },
        onUpsert: (incarnationId) => {
          if (incarnationId) descriptors.observeIncarnation(incarnationId);
          failed = false;
          // Upserts of one batch arrive as separate messages; one publish covers them all.
          if (ready) pending ??= setTimeout(publish, 0);
        },
      },
    );

    return () => {
      clearTimeout(pending);
      stop();
    };
    // Keyed by the query and range values, so equal literals do not restart the stream.
  }, [active, client, durationMs, identity, queriesKey, select, stepMs]);

  // A stopped watch forgets what it read, so reopening it never shows grids from a stale window.
  // A paused one keeps its last answer, which stays in view after resuming until the next one.
  if (!watching && (state || kept)) {
    setState(undefined);
    setKept(undefined);
  }
  const current = watching && state?.identity === identity ? state.value : undefined;
  if (current?.value !== undefined && current.value !== kept?.value) {
    setKept({ queriesKey, value: current.value });
  }
  if (!watching) return undefined;
  // Previous grids of the same queries stay in view until a changed range answers.
  if (current?.value !== undefined || kept?.queriesKey !== queriesKey) {
    return current ?? { failed: false };
  }

  return { failed: current?.failed ?? false, value: kept.value };
}

function selectGrids(accumulators: readonly MetricWatchAccumulator[]): MetricGrid[] {
  return accumulators.flatMap((accumulator) => accumulator.grid() ?? []);
}

function selectAnswers(accumulators: readonly MetricWatchAccumulator[]): MetricAnswer[] {
  return accumulators.map((accumulator) => ({
    grid: accumulator.grid(),
    stats: accumulator.statsGrid(),
  }));
}

interface WatchCallbacks {
  onBackfillComplete: () => void;
  onFailure: () => void;
  onUpsert: (incarnationId: string | undefined) => void;
}

/**
 * Keeps one watch stream open for all queries, routing each keyed upsert into its query's
 * accumulator; every open backfills the full range, merged over the held points. Reconnection,
 * hidden-tab closing and rejected queries (invalid argument or unknown metric, not retried) are
 * the shared stream loop's. Returns the function that stops it.
 */
function watchQueries(
  client: MetricsClient,
  queries: MetricQuery[],
  accumulators: MetricWatchAccumulator[],
  callbacks: WatchCallbacks,
): () => void {
  const keyedQueries = queries.map((query, index) =>
    create(KeyedMetricQuerySchema, { key: String(index), query }),
  );

  return runStreamLoop<WatchMetricsResponse>({
    onFailure: callbacks.onFailure,
    onMessage: (response, markLive) => {
      if (response.event.case === "upsert") {
        const { key, result } = response.event.value;
        if (result) accumulators[Number(key)]?.apply(result);
        callbacks.onUpsert(result?.incarnation?.id);
      } else if (response.event.case === "backfillComplete") {
        markLive();
        callbacks.onBackfillComplete();
      }
    },
    open: (signal) => client.metrics.watchMetrics({ queries: keyedQueries }, { signal }),
  });
}

function toMetricQuery(query: MetricHistoryQuery, durationMs: number, stepMs: number) {
  return create(MetricQuerySchema, {
    aggregate: query.aggregate ?? MetricAggregate.SUM,
    groupBy: [...query.groupBy],
    matchers: { ...query.matchers },
    metric: query.metric,
    percentiles: [...(query.percentiles ?? [])],
    range: { case: "last", value: toDuration(durationMs) },
    reduce: query.reduce ?? MetricReduce.LAST,
    stepHint: toDuration(stepMs),
  });
}

function toDuration(ms: number) {
  return create(DurationSchema, {
    nanos: Math.round((ms % 1000) * 1e6),
    seconds: BigInt(Math.floor(ms / 1000)),
  });
}
