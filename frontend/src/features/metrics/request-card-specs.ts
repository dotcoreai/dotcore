import type { ChartColor, ChartSeries } from "@/components/metrics/time-series-chart/chart-data";
import { MetricReduce } from "@/gen/dotcore/metrics/v1/metric_pb";

import type { MetricStatsGrid } from "./metric-grid";
import { dimensionNames, metricNames } from "./metric-names";
import { counterTotal, perStepValues, weightedMean } from "./metric-range-values";
import type { MetricCardSpec, MetricCardUnit } from "./use-metric-cards";
import type { MetricAnswer, MetricHistoryQuery } from "./use-metric-watch";

/**
 * What request cards read for one subject: the label filter naming it, the counter of the requests
 * it ended, and the time-to-first-token distribution measured for it.
 */
export interface RequestCardSubject {
  matchers: Readonly<Record<string, string>>;
  /** Counter of ended requests, by outcome. */
  requests: string;
  ttft: string;
}

/** The ids of the request cards. */
export const requestCardIds = {
  generatedTokens: "generated-tokens",
  promptProcessing: "prompt-processing",
  promptTokens: "prompt-tokens",
  requests: "requests",
  tokenGeneration: "token-generation",
  ttft: "ttft",
} as const;

/**
 * The request cards in the order every view shows them: how much the subject served, then how fast,
 * in the order a request goes through prompt processing, its first token and token generation.
 */
export const requestCardOrder = [
  requestCardIds.requests,
  requestCardIds.promptTokens,
  requestCardIds.generatedTokens,
  requestCardIds.promptProcessing,
  requestCardIds.ttft,
  requestCardIds.tokenGeneration,
] as const;

/**
 * A model deployment's request cards. A deployment counts attempts, so retries and fallbacks that
 * reach it count too, and its time to first token excludes earlier attempts of the request.
 */
export function deploymentRequestCards(deploymentId: string): MetricCardSpec[] {
  return requestCardSpecs({
    matchers: { [dimensionNames.deployment]: deploymentId },
    requests: metricNames.routerAttempts,
    ttft: metricNames.routerAttemptTtft,
  });
}

/** An inference route's request cards, counting the requests it routed from their start. */
export function routeRequestCards(routeId: string): MetricCardSpec[] {
  return requestCardSpecs({
    matchers: { [dimensionNames.route]: routeId },
    requests: metricNames.routerRequests,
    ttft: metricNames.routerRequestTtft,
  });
}

/**
 * The six request cards of a subject over the range, in `requestCardOrder`: requests ended, prompt
 * tokens with the share served from cache, generated tokens with the average per successful
 * request, then the mean prompt processing speed, time to first token and token generation speed.
 * Counts total their steps and read zero when idle; speeds weigh every sample alike and read no
 * data when none was taken.
 */
export function requestCardSpecs(subject: RequestCardSubject): MetricCardSpec[] {
  return [
    requestsCard(subject),
    promptTokensCard(subject.matchers),
    generatedTokensCard(subject),
    promptProcessingCard(subject.matchers),
    ttftCard(subject),
    tokenGenerationCard(subject.matchers),
  ];
}

/** The requests the subject ended over the range, as bars. */
function requestsCard(subject: Pick<RequestCardSubject, "matchers" | "requests">): MetricCardSpec {
  return countCard({
    chart: ([requests]) => [bars("requests", "Requests", "chart-1", requests)],
    id: requestCardIds.requests,
    label: "Requests",
    queries: [counterQuery(subject.requests, { ...subject.matchers })],
    read: ([requests]) => countFigures(requests),
  });
}

/** Prompt tokens over the range, with the share served from cache below. */
function promptTokensCard(matchers: Readonly<Record<string, string>>): MetricCardSpec {
  return countCard({
    chart: ([prompt, cached]) => [
      bars("prompt", "Processed", "chart-1", prompt),
      bars("cached", "Cached", "chart-2", cached),
    ],
    id: requestCardIds.promptTokens,
    label: "Processed tokens",
    queries: [
      counterQuery(metricNames.routerTokensPrompt, { ...matchers }),
      counterQuery(metricNames.routerTokensCachedPrompt, { ...matchers }),
    ],
    read: ([prompt, cached]) => {
      const figures = countFigures(prompt);
      return { ...figures, line: ratioLine("cached", "ratio", total(cached), figures.value) };
    },
  });
}

/** Generated tokens over the range, with the average per successful request below. */
function generatedTokensCard(
  subject: Pick<RequestCardSubject, "matchers" | "requests">,
): MetricCardSpec {
  return countCard({
    chart: ([generated]) => [bars("generated", "Generated", "chart-1", generated)],
    id: requestCardIds.generatedTokens,
    label: "Generated tokens",
    queries: [
      counterQuery(metricNames.routerTokensCompletion, { ...subject.matchers }),
      counterQuery(subject.requests, {
        ...subject.matchers,
        [dimensionNames.outcome]: "success",
      }),
    ],
    read: ([generated, succeeded]) => {
      const figures = countFigures(generated);
      return {
        ...figures,
        line: ratioLine("per request", "count", figures.value, total(succeeded)),
      };
    },
  });
}

/** The mean prompt processing speed. */
function promptProcessingCard(matchers: Readonly<Record<string, string>>): MetricCardSpec {
  return speedCard(requestCardIds.promptProcessing, "Prompt processing", "tokensPerSecond", {
    matchers,
    metric: metricNames.routerAttemptPrefillRate,
  });
}

/** The mean token generation speed. */
function tokenGenerationCard(matchers: Readonly<Record<string, string>>): MetricCardSpec {
  return speedCard(requestCardIds.tokenGeneration, "Token generation", "tokensPerSecond", {
    matchers,
    metric: metricNames.routerAttemptDecodeRate,
  });
}

/** The mean time to first token. */
function ttftCard(subject: Pick<RequestCardSubject, "matchers" | "ttft">): MetricCardSpec {
  return speedCard(requestCardIds.ttft, "Time to first token", "seconds", {
    matchers: subject.matchers,
    metric: subject.ttft,
  });
}

/** A distribution's per-bucket mean and p95 over the series matching the labels. */
export function distributionQuery(
  metric: string,
  matchers: Readonly<Record<string, string>> = {},
): MetricHistoryQuery {
  return { groupBy: [], matchers: { ...matchers }, metric, percentiles: [chartQuantile] };
}

/** A counter summed across its series in each bucket. */
export function counterQuery(metric: string, matchers: Record<string, string>): MetricHistoryQuery {
  return { groupBy: [], matchers, metric, reduce: MetricReduce.SUM };
}

/** The quantile charted above the mean of speeds and time to first token. */
const chartQuantile = 0.95;

/** A counter card: bars of its per-step deltas, and a split group valued by its total. */
function countCard(
  spec: Pick<MetricCardSpec, "chart" | "id" | "label" | "queries" | "read">,
): MetricCardSpec {
  return {
    ...spec,
    groupValue: (series) => counterTotal(series.steps) ?? 0,
    mark: "bars",
    unit: "count",
  };
}

/** A distribution read as its mean over the range, charted as its per-step mean and p95. */
function speedCard(
  id: string,
  label: string,
  unit: MetricCardUnit,
  { matchers, metric }: { matchers: Readonly<Record<string, string>>; metric: string },
): MetricCardSpec {
  const query = distributionQuery(metric, matchers);

  return {
    chart: ([answer]) => {
      const p95 = answer?.grid?.series.find((series) => series.quantile === chartQuantile);
      return [
        {
          color: "chart-1",
          id: "mean",
          label: "Mean",
          mark: "line",
          provisional: answer?.stats?.series[0]?.provisional,
          values: meanValues(answer?.stats),
        },
        {
          color: "chart-4",
          id: "p95",
          label: "p95",
          mark: "line",
          provisional: p95?.provisional,
          values: p95?.values ?? emptySteps(answer?.stats),
        },
      ];
    },
    groupValue: (series) => weightedMean(series.steps),
    id,
    label,
    mark: "line",
    queries: [query],
    read: ([answer]) => ({
      sparkline: meanValues(answer?.stats),
      value: weightedMean(answer?.stats?.series.flatMap((series) => series.steps) ?? []),
    }),
    unit,
  };
}

/** A counter's total over the range, zero when idle, with its per-step deltas. */
function countFigures(answer: MetricAnswer | undefined) {
  return { sparkline: stepValues(answer?.stats), value: total(answer) };
}

/** `numerator ÷ denominator` as a line below; no value without a denominator. */
function ratioLine(label: string, unit: MetricCardUnit, numerator: number, denominator: number) {
  return { label, unit, value: denominator ? numerator / denominator : null };
}

/** The counter total across every series of an answer; an idle or absent counter totals zero. */
function total(answer: MetricAnswer | undefined): number {
  return (answer?.stats?.series ?? []).reduce(
    (sum, series) => sum + (counterTotal(series.steps) ?? 0),
    0,
  );
}

/** A counter's per-step deltas as bars. */
function bars(
  id: string,
  label: string,
  color: ChartColor,
  answer: MetricAnswer | undefined,
): ChartSeries {
  return {
    color,
    id,
    label,
    mark: "bars",
    provisional: answer?.stats?.series[0]?.provisional,
    values: stepValues(answer?.stats),
  };
}

/** A distribution's per-step sample means. */
function meanValues(grid: MetricStatsGrid | undefined): (number | null)[] {
  const series = grid?.series[0];
  return series ? perStepValues(series.steps) : emptySteps(grid);
}

/** Per-step counter deltas summed across the grid's series; a step no series has a value for stays absent. */
function stepValues(grid: MetricStatsGrid | undefined): (number | null)[] {
  const values = emptySteps(grid);

  for (const series of grid?.series ?? []) {
    perStepValues(series.steps).forEach((value, index) => {
      if (value !== null) values[index] = (values[index] ?? 0) + value;
    });
  }

  return values;
}

function emptySteps(grid: MetricStatsGrid | undefined): (number | null)[] {
  return Array.from({ length: grid?.stepCount ?? 0 }, () => null);
}
