import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { MetricsClient } from "@/api/metrics-client";
import { inferenceCardIds } from "@/features/metrics/inference/inference-card-specs";
import { InferenceSection } from "@/features/metrics/inference/inference-section";
import {
  counterPoint,
  distributionPoint,
  gridStartSeconds,
  metricsClientWith,
  metricSeries,
  queryResult,
  staticWatch,
} from "@/features/metrics/metric-test-support";
import { type MetricCards, useMetricCards } from "@/features/metrics/use-metric-cards";
import { InferenceRouteViewSchema } from "@/gen/dotcore/inference/v1/routes_pb";
import type { MetricQuerySchema } from "@/gen/dotcore/metrics/v1/metric_pb";
import { ModelDeploymentSchema } from "@/gen/dotcore/model/v1/deployment_pb";

import { overviewCardSpecs, overviewSplitIds, overviewSplits } from "./overview-card-specs";

vi.mock("uplot", () => ({
  default: class {
    static paths = { bars: () => undefined };

    setData = vi.fn();
    redraw = vi.fn();
    setSize = vi.fn();
    destroy = vi.fn();
  },
}));

const deployments = [
  create(ModelDeploymentSchema, { modelDeploymentId: "dep-a", modelDeploymentName: "Gemma" }),
  create(ModelDeploymentSchema, { modelDeploymentId: "dep-b", modelDeploymentName: "Qwen" }),
];
const routes = [
  create(InferenceRouteViewSchema, { route: { inferenceRouteId: "route-a", name: "chat" } }),
];
const lastStep = 59 * 15;

describe("OverviewInferenceSection", () => {
  // The clock sits at the end of the fixtures' range, inside their open last step.
  beforeEach(() => {
    vi.useFakeTimers({
      now: (gridStartSeconds + 60 * 15) * 1000,
      shouldAdvanceTime: true,
      toFake: ["Date"],
    });
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("shows the six site cards in the sidebars' order with their values over the range", async () => {
    renderSection(metricsClientWith(staticWatch(busyAnswer)));

    const section = screen.getByRole("region", { name: "Inference" });
    await waitFor(() => expect(cardTexts(section)[0]).not.toBe("Requests | —"));
    expect(cardTexts(section)).toEqual([
      "Requests | 5",
      "Processed tokens | 150 | 40% cached",
      "Generated tokens | 40 | 10 per request",
      "Prompt processing | 467 tokens/s",
      "Time to first token | 250 ms",
      "Token generation | 50 tokens/s",
    ]);
    expect(screen.queryByRole("heading", { level: 4 })).toBeNull();
  });

  it("opens one stream for every card and split, and never reopens it for a card or split", async () => {
    const user = userEvent.setup();
    const watchMetrics = vi.fn(staticWatch(busyAnswer));
    const { metrics } = renderSection(metricsClientWith(watchMetrics));
    await waitFor(() => expect(metrics.current.cards[0].reading).toBeDefined());

    // Eight inference queries for six cards, three app queries for three cards and seventeen
    // machine queries for seven cards (a mean and an extreme per gauge), plus each inference card
    // regrouped for the model and route splits; the app and machine cards chart their split from
    // their own answers.
    expect(watchMetrics).toHaveBeenCalledTimes(1);
    const queries = watchMetrics.mock.calls[0][0].queries ?? [];
    expect(queries.length).toBe(8 + 3 + 17 + inferenceCardIds.length * 2);
    expect(overviewCardSpecs().reduce((count, spec) => count + spec.queries.length, 0)).toBe(28);

    await user.click(screen.getByRole("button", { name: /^Requests/ }));
    await user.click(screen.getByRole("radio", { name: "Total" }));
    await user.click(screen.getByRole("radio", { name: "By route" }));
    await user.click(screen.getByRole("button", { name: /^Token generation/ }));

    expect(watchMetrics).toHaveBeenCalledTimes(1);
  });

  it("charts requests by model as attempts per deployment name, the default split", async () => {
    const user = userEvent.setup();
    const { metrics } = renderSection(metricsClientWith(staticWatch(busyAnswer)));
    await waitFor(() => expect(metrics.current.cards[0].reading).toBeDefined());

    await user.click(screen.getByRole("button", { name: /^Requests/ }));

    expect(screen.getByRole("radio", { name: "By model" })).toBeChecked();
    expect(chartSeries(metrics.current)).toEqual([
      { label: "Gemma", mark: "stackedBars", values: { 0: 1, 59: 2 } },
      { label: "Qwen", mark: "stackedBars", values: { 0: 2 } },
    ]);

    await user.click(screen.getByRole("radio", { name: "By route" }));
    expect(chartSeries(metrics.current)).toEqual([
      { label: "chat", mark: "stackedBars", values: { 0: 3 } },
    ]);

    await user.click(screen.getByRole("radio", { name: "Total" }));
    expect(chartSeries(metrics.current)).toEqual([
      { label: "Requests", mark: "bars", values: { 0: 3, 59: 2 } },
    ]);
  });

  it("names a group the console no longer lists by its id", async () => {
    const user = userEvent.setup();
    const { metrics } = renderSection(metricsClientWith(staticWatch(busyAnswer)), {
      deployments: [],
    });
    await waitFor(() => expect(metrics.current.cards[0].reading).toBeDefined());

    await user.click(screen.getByRole("button", { name: /^Requests/ }));

    expect(chartSeries(metrics.current)?.map((series) => series.label)).toEqual(["dep-a", "dep-b"]);
  });

  it("shows zeros and dashes on an idle range, with no chart until a card is selected", async () => {
    renderSection(
      metricsClientWith(
        staticWatch(() => queryResult({ series: [], stepCount: 60, stepSeconds: 15 })),
      ),
    );

    const section = screen.getByRole("region", { name: "Inference" });
    await waitFor(() => expect(cardTexts(section)[0]).toBe("Requests | 0"));
    expect(cardTexts(section)).toEqual([
      "Requests | 0",
      "Processed tokens | 0",
      "Generated tokens | 0",
      "Prompt processing | —",
      "Time to first token | —",
      "Token generation | —",
    ]);
  });

  it("says metrics could not be read when no range is served", async () => {
    const client = metricsClientWith(staticWatch(busyAnswer));
    client.metrics.listMetricDescriptors = () => Promise.reject(new Error("down"));
    renderSection(client);

    expect(await screen.findByText("Inference metrics could not be read.")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^Requests/ })).toBeNull();
  });
});

function renderSection(
  client: MetricsClient,
  options: { deployments?: typeof deployments } = {},
): { metrics: { current: MetricCards } } {
  const metrics = {} as { current: MetricCards };
  const specs = overviewCardSpecs();
  const splits = overviewSplits(options.deployments ?? deployments, routes);
  function Page() {
    metrics.current = useMetricCards(client, specs, {
      initialSplit: overviewSplitIds.model,
      splits,
    });
    return <InferenceSection metrics={metrics.current} />;
  }
  act(() => {
    render(<Page />);
  });

  return { metrics };
}

/** Each card as "label | value unit | line" from its rendered text. */
function cardTexts(section: HTMLElement): string[] {
  return within(section)
    .getAllByRole("button", { pressed: false })
    .map((card) =>
      [
        card.querySelector('[data-slot="metric-card-label"]'),
        card.querySelector('[data-slot="metric-card-value"]'),
        card.querySelector('[data-slot="metric-card-line"]'),
      ]
        .map((part) => part?.textContent?.replace(/\s+/g, " ").trim())
        .filter(Boolean)
        .join(" | "),
    );
}

function chartSeries(cards: MetricCards) {
  return cards.chart?.series.map((series) => ({
    label: series.label,
    mark: series.mark ?? "line",
    values: Object.fromEntries(
      series.values.flatMap((value, index) => (value === null ? [] : [[index, value]])),
    ),
  }));
}

/**
 * Site traffic: three requests then two (open step), spread over two deployments and one route;
 * 40 completion tokens, with speeds and time to first token.
 */
function busyAnswer(query: MessageInitShape<typeof MetricQuerySchema> | undefined) {
  const groupBy = query?.groupBy ?? [];
  const counter = (first: number, last?: number, labels: Record<string, string> = {}) =>
    metricSeries(labels, [
      counterPoint(0, 0, false, first),
      ...(last === undefined ? [] : [counterPoint(lastStep, 0, true, last)]),
    ]);
  const series = (() => {
    switch (query?.metric) {
      case "router.requests":
        if (groupBy.includes("route")) return [counter(3, undefined, { route: "route-a" })];
        return query?.matchers?.outcome === "success" ? [counter(4)] : [counter(3, 2)];
      case "router.attempts":
        return [
          counter(1, 2, { deployment: "dep-a" }),
          counter(2, undefined, { deployment: "dep-b" }),
        ];
      case "router.tokens.prompt":
        return [counter(100, 50)];
      case "router.tokens.cached_prompt":
        return [counter(60)];
      case "router.tokens.completion":
        if (groupBy.includes("deployment")) {
          return [
            counter(30, undefined, { deployment: "dep-a" }),
            counter(10, undefined, { deployment: "dep-b" }),
          ];
        }
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
      case "router.request.ttft":
      case "router.attempt.ttft":
        return [
          metricSeries({}, [distributionPoint(0, { 0.95: 0.4 }, false, { count: 2, sum: 0.5 })]),
        ];
      default:
        return [];
    }
  })();

  return queryResult({ series, stepCount: 60, stepSeconds: 15 });
}
