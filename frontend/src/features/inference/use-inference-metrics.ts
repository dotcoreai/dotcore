import { useMemo } from "react";

import type { MetricsClient } from "@/api/metrics-client";
import { useWatchedModelDeployments, useWatchedRoutes } from "@/app/resource-watch";
import { dimensionNames } from "@/features/metrics/metric-names";
import { requestCardIds } from "@/features/metrics/request-card-specs";
import { type MetricCards, useMetricCards } from "@/features/metrics/use-metric-cards";

import {
  inferenceCardSpecs,
  type InferenceScope,
  inferenceSplitIds,
  inferenceSplits,
} from "./inference-card-specs";

/** A page's inference cards, and each listed item's own traffic over the same range. */
export interface InferenceMetrics extends MetricCards {
  /**
   * One item's traffic over the selected range: a model deployment's by its id on the deployments
   * scope, a route's by its id on the routes scope. Absent until the cards are first read.
   */
  rowValues: (id: string) => InferenceRowValues | undefined;
}

/**
 * An item's traffic over the range. Counts read zero when it had none; speeds and time to first
 * token are `null` without a sample.
 */
export interface InferenceRowValues {
  generatedTokens: number;
  requests: number;
  /** Mean generation speed, in tokens per second. */
  tokenGeneration: number | null;
  /** Mean time to first token, in seconds. */
  ttft: number | null;
}

/**
 * The metric state of a page listing model deployments or routes: the site's inference cards at
 * the page range in one stream, split by model by default, and each item's traffic read from the
 * same stream through the split matching the scope. Splits are named from the watched deployments
 * and routes.
 */
export function useInferenceMetrics(
  client: MetricsClient,
  scope: InferenceScope,
): InferenceMetrics {
  const deployments = useWatchedModelDeployments()?.value?.deployments;
  const routes = useWatchedRoutes()?.value?.routes;
  const specs = useMemo(() => inferenceCardSpecs(scope), [scope]);
  const splits = useMemo(() => inferenceSplits(deployments, routes), [deployments, routes]);
  const metrics = useMetricCards(client, specs, {
    initialSplit: inferenceSplitIds.model,
    splits,
  });
  const rows = inferenceRowValues(metrics, scope);

  return { ...metrics, rowValues: (id) => rows?.get(id) ?? (rows ? idleRow : undefined) };
}

/** An item missing from the answer had no traffic: no requests, but no speed either. */
const idleRow: InferenceRowValues = {
  generatedTokens: 0,
  requests: 0,
  tokenGeneration: null,
  ttft: null,
};

/** Each item's traffic by id from the split of the scope; absent until the cards are first read. */
function inferenceRowValues(
  metrics: MetricCards,
  scope: InferenceScope,
): ReadonlyMap<string, InferenceRowValues> | undefined {
  if (!metrics.cards.some((card) => card.reading !== undefined)) return undefined;

  const [splitId, dimension] =
    scope === "deployments"
      ? [inferenceSplitIds.model, dimensionNames.deployment]
      : [inferenceSplitIds.route, dimensionNames.route];
  const byId = (cardId: string) =>
    new Map(
      metrics.groupValues(splitId, cardId).flatMap((group) => {
        const id = group.labels[dimension];
        return id === undefined ? [] : [[id, group.value] as const];
      }),
    );
  const requests = byId(requestCardIds.requests);
  const generatedTokens = byId(requestCardIds.generatedTokens);
  const tokenGeneration = byId(requestCardIds.tokenGeneration);
  const ttft = byId(requestCardIds.ttft);
  const ids = new Set([
    ...requests.keys(),
    ...generatedTokens.keys(),
    ...tokenGeneration.keys(),
    ...ttft.keys(),
  ]);

  return new Map(
    [...ids].map((id) => [
      id,
      {
        generatedTokens: generatedTokens.get(id) ?? 0,
        requests: requests.get(id) ?? 0,
        tokenGeneration: tokenGeneration.get(id) ?? null,
        ttft: ttft.get(id) ?? null,
      },
    ]),
  );
}
