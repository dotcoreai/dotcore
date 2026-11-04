import { describe, expect, it } from "vitest";

import { MetricReduce } from "@/gen/dotcore/metrics/v1/metric_pb";

import { fillMetricGrid, fillMetricStatsGrid } from "./metric-grid";
import { counterPoint, distributionPoint, metricSeries, queryResult } from "./metric-test-support";
import { deploymentRequestCards, requestCardIds, routeRequestCards } from "./request-card-specs";
import type { MetricAnswer } from "./use-metric-watch";

describe("request card specs", () => {
  it("offers the same six cards in the same order for deployments and routes", () => {
    const labels = [
      "Requests",
      "Processed tokens",
      "Generated tokens",
      "Prompt processing",
      "Time to first token",
      "Token generation",
    ];

    expect(deploymentRequestCards("deployment-a").map((card) => card.label)).toEqual(labels);
    expect(routeRequestCards("route-a").map((card) => card.label)).toEqual(labels);
  });

  it("counts attempts and attempt TTFT of a deployment", () => {
    expect(cardQueries(deploymentRequestCards("deployment-a"))).toEqual({
      "generated-tokens": [
        "router.tokens.completion deployment=deployment-a sum",
        "router.attempts deployment=deployment-a,outcome=success sum",
      ],
      "prompt-processing": ["router.attempt.prefill_rate deployment=deployment-a p95"],
      "prompt-tokens": [
        "router.tokens.prompt deployment=deployment-a sum",
        "router.tokens.cached_prompt deployment=deployment-a sum",
      ],
      requests: ["router.attempts deployment=deployment-a sum"],
      "token-generation": ["router.attempt.decode_rate deployment=deployment-a p95"],
      ttft: ["router.attempt.ttft deployment=deployment-a p95"],
    });
  });

  it("counts requests and request TTFT of a route", () => {
    expect(cardQueries(routeRequestCards("route-a"))).toEqual({
      "generated-tokens": [
        "router.tokens.completion route=route-a sum",
        "router.requests outcome=success,route=route-a sum",
      ],
      "prompt-processing": ["router.attempt.prefill_rate route=route-a p95"],
      "prompt-tokens": [
        "router.tokens.prompt route=route-a sum",
        "router.tokens.cached_prompt route=route-a sum",
      ],
      requests: ["router.requests route=route-a sum"],
      "token-generation": ["router.attempt.decode_rate route=route-a p95"],
      ttft: ["router.request.ttft route=route-a p95"],
    });
  });

  it("charts prompt tokens as bars with the cached share drawn over them", () => {
    const [, prompt] = deploymentRequestCards("deployment-a");
    const answers = [answer(10, 7), answer(4)];

    expect(
      prompt
        .chart(answers)
        .map(({ color, label, mark, values }) => ({ color, label, mark, values })),
    ).toEqual([
      { color: "chart-1", label: "Processed", mark: "bars", values: [10, 7] },
      { color: "chart-2", label: "Cached", mark: "bars", values: [4, null] },
    ]);
  });

  it("values a split group by the card's own rule: a total or a mean", () => {
    const counter = fillMetricStatsGrid(
      queryResult({
        series: [
          metricSeries({ deployment: "a" }, [
            counterPoint(0, 0, false, 30),
            counterPoint(10, 0, true, 6),
          ]),
        ],
        stepCount: 2,
        stepSeconds: 10,
      }),
    );
    const latency = fillMetricStatsGrid(
      queryResult({
        series: [
          metricSeries({ deployment: "a" }, [
            distributionPoint(0, {}, false, { count: 1, sum: 0.1 }),
            distributionPoint(1, {}, false, { count: 3, sum: 0.9 }),
          ]),
        ],
        stepCount: 2,
      }),
    );
    const cards = deploymentRequestCards("a");
    const card = (id: string) => cards.find((spec) => spec.id === id);

    expect(card(requestCardIds.requests)?.groupValue?.(counter.series[0], counter)).toBe(36);
    expect(card(requestCardIds.ttft)?.groupValue?.(latency.series[0], latency)).toBe(0.25);
  });

  it("never names prefill or decode in card or chart copy", () => {
    const answers = Array.from({ length: 2 }, () => answer(1));
    const copy = [...deploymentRequestCards("d"), ...routeRequestCards("r")].flatMap((card) => [
      card.label,
      card.read(answers).line?.label ?? "",
      ...card.chart(answers).map((series) => series.label),
    ]);

    expect(copy.filter((text) => /prefill|decode/i.test(text))).toEqual([]);
  });
});

/** Each card's queries, as metric, sorted matchers, and how they reduce. */
function cardQueries(cards: ReturnType<typeof routeRequestCards>) {
  return Object.fromEntries(
    cards.map((card) => [
      card.id,
      card.queries.map((query) => {
        const matchers = Object.entries(query.matchers)
          .sort(([a], [b]) => a.localeCompare(b))
          .map(([name, value]) => `${name}=${value}`)
          .join(",");
        const reads =
          query.reduce === MetricReduce.SUM
            ? "sum"
            : (query.percentiles ?? []).map((quantile) => `p${quantile * 100}`).join(",");
        return `${query.metric} ${matchers} ${reads}`;
      }),
    ]),
  );
}

/** A counter answer over two one-second steps, the second one absent when not given. */
function answer(first: number, second?: number): MetricAnswer {
  const result = queryResult({
    series: [
      metricSeries({}, [
        counterPoint(0, first),
        ...(second === undefined ? [] : [counterPoint(1, second)]),
      ]),
    ],
    stepCount: 2,
  });

  return { grid: fillMetricGrid(result), stats: fillMetricStatsGrid(result) };
}
