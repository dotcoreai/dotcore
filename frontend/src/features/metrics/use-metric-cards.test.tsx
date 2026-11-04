import type { MessageInitShape } from "@bufbuild/protobuf";
import { act, render, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import type { MetricsClient } from "@/api/metrics-client";
import type { MetricQuerySchema } from "@/gen/dotcore/metrics/v1/metric_pb";

import { MetricPauseContext } from "./metric-pause";
import {
  counterPoint,
  distributionPoint,
  gridStartSeconds,
  metricsClientWith,
  metricSeries,
  queryResult,
  staticWatch,
} from "./metric-test-support";
import { deploymentRequestCards } from "./request-card-specs";
import { type MetricCards, type MetricCardSplit, useMetricCards } from "./use-metric-cards";

const specs = deploymentRequestCards("deployment-a");
const splits: MetricCardSplit[] = [{ groupBy: ["model"], id: "model", label: "By model" }];
const lastStep = 59 * 15;

describe("useMetricCards", () => {
  it("opens one stream carrying every card's queries at the finest card range", async () => {
    const watchMetrics = vi.fn(staticWatch(busyAnswer));
    const cards = renderCards(metricsClientWith(watchMetrics));

    await waitFor(() => expect(cards.current.cards[0].reading).toBeDefined());
    expect(watchMetrics).toHaveBeenCalledTimes(1);
    expect(openStreams(watchMetrics)).toEqual([
      [
        "router.attempts",
        "router.tokens.prompt",
        "router.tokens.cached_prompt",
        "router.tokens.completion",
        "router.attempts",
        "router.attempt.prefill_rate",
        "router.attempt.ttft",
        "router.attempt.decode_rate",
      ],
    ]);
    const query = watchMetrics.mock.calls[0][0].queries?.[0].query;
    expect(query?.range).toMatchObject({ case: "last", value: { seconds: 900n } });
    expect(query?.stepHint?.seconds).toBe(15n);
    expect(cards.current.range?.label).toBe("15m");
  });

  it("reads totals over every step and means weighted by sample count", async () => {
    const cards = renderCards(metricsClientWith(staticWatch(busyAnswer)));

    await waitFor(() => expect(cards.current.cards[0].reading).toBeDefined());
    expect(readings(cards.current)).toEqual({
      "prompt-processing": { line: undefined, value: 1400 / 3 },
      "prompt-tokens": { line: { label: "cached", unit: "ratio", value: 0.4 }, value: 150 },
      "generated-tokens": {
        line: { label: "per request", unit: "count", value: 10 },
        value: 40,
      },
      requests: { line: undefined, value: 5 },
      "token-generation": { line: undefined, value: 50 },
      ttft: { line: undefined, value: 0.25 },
    });
    const requests = cards.current.cards[0];
    expect(requests.reading?.historyStartMs).toBeUndefined();
    expect(requests.reading?.sparkline[0]).toBe(3);
    expect(requests.reading?.sparkline[59]).toBe(2);
    expect(requests.reading?.sparkline.slice(1, 59).every((value) => value === null)).toBe(true);
  });

  it("reads zero counts and no data for means over an idle range", async () => {
    const cards = renderCards(
      metricsClientWith(
        staticWatch(() => queryResult({ series: [], stepCount: 60, stepSeconds: 15 })),
      ),
    );

    await waitFor(() => expect(cards.current.cards[0].reading).toBeDefined());
    expect(readings(cards.current)).toEqual({
      "prompt-processing": { line: undefined, value: null },
      "prompt-tokens": { line: { label: "cached", unit: "ratio", value: null }, value: 0 },
      "generated-tokens": { line: { label: "per request", unit: "count", value: null }, value: 0 },
      requests: { line: undefined, value: 0 },
      "token-generation": { line: undefined, value: null },
      ttft: { line: undefined, value: null },
    });
  });

  it("says where history starts when the store started inside the range", async () => {
    const cards = renderCards(
      metricsClientWith(staticWatch((query) => busyAnswer(query, { startedAtOffset: 300 }))),
    );

    await waitFor(() => expect(cards.current.cards[0].reading).toBeDefined());
    expect(cards.current.cards.map((card) => card.reading?.historyStartMs)).toEqual(
      specs.map(() => (gridStartSeconds + 300) * 1000),
    );
  });

  it("charts a selected card and its split from the same stream", async () => {
    const watchMetrics = vi.fn(staticWatch(busyAnswer));
    const cards = renderCards(metricsClientWith(watchMetrics), { splits });

    await waitFor(() => expect(cards.current.cards[0].reading).toBeDefined());
    expect(cards.current.chart).toBeUndefined();
    // Each card's first query is also read grouped by model.
    const queries = watchMetrics.mock.calls[0][0].queries ?? [];
    expect(queries).toHaveLength(14);
    expect(queries[8].query).toMatchObject({ groupBy: ["model"], metric: "router.attempts" });

    act(() => cards.current.setSelected("requests"));
    expect(cards.current.chart).toMatchObject({
      historyStartMs: gridStartSeconds * 1000,
      id: "requests",
      startMs: gridStartSeconds * 1000,
      stepCount: 60,
      stepMs: 15_000,
      title: "Requests",
      unit: "requests",
    });
    expect(chartSeries(cards.current)).toEqual([
      { label: "Requests", mark: "bars", values: { 0: 3, 59: 2 } },
    ]);

    act(() => cards.current.setSplit("model"));
    expect(cards.current.split).toBe("model");
    expect(chartSeries(cards.current)).toEqual([
      { label: "model-a", mark: "stackedBars", values: { 0: 1, 59: 2 } },
      { label: "model-b", mark: "stackedBars", values: { 0: 2 } },
    ]);

    act(() => cards.current.setSplit(undefined));
    act(() => cards.current.setSelected(undefined));
    expect(cards.current.chart).toBeUndefined();
    expect(watchMetrics).toHaveBeenCalledTimes(1);
  });

  it("charts speeds as mean and p95 lines", async () => {
    const cards = renderCards(metricsClientWith(staticWatch(busyAnswer)));
    await waitFor(() => expect(cards.current.cards[0].reading).toBeDefined());

    act(() => cards.current.setSelected("token-generation"));

    expect(cards.current.chart?.unit).toBe("tokensPerSecond");
    expect(chartSeries(cards.current)).toEqual([
      { label: "Mean", mark: "line", values: { 0: 50 } },
      { label: "p95", mark: "line", values: { 0: 90 } },
    ]);
  });

  it("keeps a card's reading while its grids are unchanged", async () => {
    const cards = renderCards(metricsClientWith(staticWatch(busyAnswer)));
    await waitFor(() => expect(cards.current.cards[0].reading).toBeDefined());
    const before = cards.current.cards[0].reading;

    act(() => cards.current.setSelected("requests"));

    expect(cards.current.cards[0].reading).toBe(before);
  });

  it("reopens the stream once at a picked range", async () => {
    const watchMetrics = vi.fn(staticWatch(busyAnswer));
    const cards = renderCards(metricsClientWith(watchMetrics));
    await waitFor(() => expect(cards.current.ranges).toHaveLength(2));

    act(() => cards.current.setRange("6h"));

    await waitFor(() => expect(watchMetrics).toHaveBeenCalledTimes(2));
    expect(openStreams(watchMetrics)).toHaveLength(1);
    const query = watchMetrics.mock.calls[1][0].queries?.[0].query;
    expect(query?.range).toMatchObject({ case: "last", value: { seconds: 21_600n } });
    expect(query?.stepHint?.seconds).toBe(360n);
    expect(cards.current.range?.label).toBe("6h");
  });

  it("starts at the initial range", async () => {
    const watchMetrics = vi.fn(staticWatch(busyAnswer));
    const cards = renderCards(metricsClientWith(watchMetrics), { initialRange: "6h" });

    await waitFor(() => expect(watchMetrics).toHaveBeenCalledTimes(1));
    expect(watchMetrics.mock.calls[0][0].queries?.[0].query?.stepHint?.seconds).toBe(360n);
    expect(cards.current.range?.label).toBe("6h");
  });

  it("holds no stream while a log viewer pauses metrics", async () => {
    const watchMetrics = vi.fn(staticWatch(busyAnswer));
    const cards = renderCards(metricsClientWith(watchMetrics), {}, true);

    await waitFor(() => expect(cards.current.ranges).toHaveLength(2));
    expect(watchMetrics).not.toHaveBeenCalled();
    expect(cards.current.cards.map((card) => card.reading)).toEqual(specs.map(() => undefined));
  });
});

/** Renders the hook and returns a handle on its latest result. */
function renderCards(
  client: MetricsClient,
  options: Parameters<typeof useMetricCards>[2] = {},
  pausedByLogs = false,
): { current: MetricCards } {
  const handle = {} as { current: MetricCards };
  function Probe() {
    handle.current = useMetricCards(client, specs, options);
    return null;
  }
  render(
    <MetricPauseContext.Provider value={pausedByLogs}>
      <Probe />
    </MetricPauseContext.Provider>,
  );

  return handle;
}

/**
 * A deployment that served requests at the first and the last (still open) step of the range:
 * three then two attempts, all successful, with their tokens and speeds.
 */
function busyAnswer(
  query: MessageInitShape<typeof MetricQuerySchema> | undefined,
  { startedAtOffset = 0 } = {},
) {
  const grouped = (query?.groupBy ?? []).includes("model");
  const counter = (first: number, last?: number) =>
    metricSeries({}, [
      counterPoint(0, 0, false, first),
      ...(last === undefined ? [] : [counterPoint(lastStep, 0, true, last)]),
    ]);
  const series = (() => {
    switch (query?.metric) {
      case "router.attempts":
        if (grouped) {
          return [
            metricSeries({ model: "model-a" }, [
              counterPoint(0, 0, false, 1),
              counterPoint(lastStep, 0, true, 2),
            ]),
            metricSeries({ model: "model-b" }, [counterPoint(0, 0, false, 2)]),
          ];
        }
        return query.matchers?.outcome === "success" ? [counter(4)] : [counter(3, 2)];
      case "router.tokens.prompt":
        return [counter(100, 50)];
      case "router.tokens.cached_prompt":
        return [counter(60)];
      case "router.tokens.completion":
        return [counter(40)];
      case "router.attempt.prefill_rate":
        return [
          metricSeries({}, [
            distributionPoint(0, { 0.95: 700 }, false, { count: 2, sum: 1000 }),
            distributionPoint(lastStep, { 0.95: 400 }, true, { count: 1, sum: 400 }),
          ]),
        ];
      case "router.attempt.decode_rate":
        return [
          metricSeries({}, [distributionPoint(0, { 0.95: 90 }, false, { count: 4, sum: 200 })]),
        ];
      case "router.attempt.ttft":
        return [
          metricSeries({}, [distributionPoint(0, { 0.95: 0.4 }, false, { count: 2, sum: 0.5 })]),
        ];
      default:
        return [];
    }
  })();

  return queryResult({ series, startedAtOffset, stepCount: 60, stepSeconds: 15 });
}

/** Each card's value and line below, by card id. */
function readings(cards: MetricCards) {
  return Object.fromEntries(
    cards.cards.map((card) => [card.id, { line: card.reading?.line, value: card.reading?.value }]),
  );
}

/** The selected chart's series, with their present values by step index. */
function chartSeries(cards: MetricCards) {
  return cards.chart?.series.map((series) => ({
    label: series.label,
    mark: series.mark ?? "line",
    values: Object.fromEntries(
      series.values.flatMap((value, index) => (value === null ? [] : [[index, value]])),
    ),
  }));
}

type WatchMock = ReturnType<typeof vi.fn<MetricsClient["metrics"]["watchMetrics"]>>;

/** The metrics of each stream still open, in opening order. */
function openStreams(watchMetrics: WatchMock): string[][] {
  return watchMetrics.mock.calls
    .filter(([, options]) => !options?.signal?.aborted)
    .map(([request]) => (request.queries ?? []).map(({ query }) => query?.metric ?? ""));
}
