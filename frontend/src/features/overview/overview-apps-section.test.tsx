import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { beforeEach, describe, expect, it, vi } from "vitest";

import type { MetricsClient } from "@/api/metrics-client";
import { MetricRangeControl } from "@/features/metrics/metric-range-control";
import {
  gaugePoint,
  metricsClientWith,
  metricSeries,
  queryResult,
  staticWatch,
} from "@/features/metrics/metric-test-support";
import { type MetricCards, useMetricCards } from "@/features/metrics/use-metric-cards";
import { ApplicationDeploymentSchema } from "@/gen/dotcore/application/v1/deployment_pb";
import { ListApplicationDeploymentsResponseSchema } from "@/gen/dotcore/application/v1/service_pb";
import type { MetricQuerySchema } from "@/gen/dotcore/metrics/v1/metric_pb";
import { ResourceWatchProvider } from "@/resource-watch/resource-watch-provider";
import { FakeResourceWatch } from "@/test/fake-resource-watch";

import type { MachineFacts } from "./machine-site-values";
import { overviewCardSpecs, overviewSplitIds, overviewSplits } from "./overview-card-specs";
import { OverviewAppsSection } from "./overview-count-cards";

vi.mock("uplot", () => ({
  default: class {
    static paths = { bars: () => undefined };

    setData = vi.fn();
    redraw = vi.fn();
    setSize = vi.fn();
    destroy = vi.fn();
  },
}));

const GIB = 1024 ** 3;
// node-a has 8 logical CPUs and node-b 24; node-c reports no CPU count.
const facts: MachineFacts = {
  cpus: new Map([
    ["node-a", 8],
    ["node-b", 24],
  ]),
  dedicated: new Set(),
  gpus: new Map(),
};
const apps = [
  create(ApplicationDeploymentSchema, {
    applicationDeploymentId: "dep-a",
    applicationDeploymentName: "Chat",
  }),
  create(ApplicationDeploymentSchema, {
    applicationDeploymentId: "dep-b",
    applicationDeploymentName: "Notes",
  }),
];

describe("OverviewAppsSection", () => {
  let streams: ReturnType<typeof countedWatch>;
  beforeEach(() => {
    streams = countedWatch();
  });

  it("shows the apps count, then CPU, RAM and VRAM over the range", async () => {
    renderSection(streams.client);

    const section = screen.getByRole("region", { name: "Apps" });
    await waitFor(() => expect(cardTexts(section)[0]).not.toBe("CPU | —"));
    // CPU is the apps' share of the hosting machines' 32 CPUs: (0.5 * 8 + 0.25 * 24 + 0.25 * 8) / 32.
    expect(cardTexts(section)).toEqual(["CPU | 38 %", "RAM | 3 GiB", "VRAM | 4 GiB"]);
    expect(within(section).getByRole("link", { name: "Apps" })).toHaveTextContent("of 2 deployed");
  });

  it("opens one stream for the page and never reopens it for a card or split", async () => {
    const user = userEvent.setup();
    const { metrics } = renderSection(streams.client);
    await waitFor(() => expect(metrics.current.cards[0].reading).toBeDefined());

    // One query per card: the split charts from the cards' own answers.
    expect(streams.watch).toHaveBeenCalledTimes(1);
    const section = screen.getByRole("region", { name: "Apps" });
    // After the 8 inference queries.
    expect(streams.queries(0).slice(8, 11)).toEqual([
      ["application.host.cpu.utilization", ["deployment", "node"]],
      ["application.host.memory.used", ["deployment"]],
      ["application.accelerator.memory.used", ["deployment"]],
    ]);

    await user.click(within(section).getByRole("button", { name: /^CPU/ }));
    await user.click(within(section).getByRole("radio", { name: "Total" }));
    await user.click(within(section).getByRole("radio", { name: "By app" }));
    await user.click(within(section).getByRole("button", { name: /^RAM/ }));

    expect(streams.watch).toHaveBeenCalledTimes(1);
  });

  it("charts each app's share of the hosting machines' CPUs, by app name", async () => {
    const user = userEvent.setup();
    const { metrics } = renderSection(streams.client);
    await waitFor(() => expect(metrics.current.cards[0].reading).toBeDefined());

    const section = screen.getByRole("region", { name: "Apps" });
    await user.click(within(section).getByRole("button", { name: /^CPU/ }));
    await user.click(within(section).getByRole("radio", { name: "By app" }));

    // A machine without a CPU count (node-c) weighs nothing.
    expect(chartSeries(metrics.current)).toEqual([
      { label: "Chat", values: { 0: 0.3125 } },
      { label: "Notes", values: { 0: 0.0625 } },
    ]);

    await user.click(within(section).getByRole("radio", { name: "Total" }));
    expect(chartSeries(metrics.current)).toEqual([{ label: "CPU", values: { 0: 0.375 } }]);
  });

  it("charts RAM and VRAM by app, summing the services of an app", async () => {
    const user = userEvent.setup();
    const { metrics } = renderSection(streams.client);
    await waitFor(() => expect(metrics.current.cards[0].reading).toBeDefined());

    const section = screen.getByRole("region", { name: "Apps" });
    await user.click(within(section).getByRole("button", { name: /^RAM/ }));
    await user.click(within(section).getByRole("radio", { name: "By app" }));
    expect(chartSeries(metrics.current)).toEqual([
      { label: "Chat", values: { 0: 2 * GIB } },
      { label: "Notes", values: { 0: GIB } },
    ]);

    await user.click(within(section).getByRole("button", { name: /^VRAM/ }));
    expect(chartSeries(metrics.current)).toEqual([{ label: "Chat", values: { 0: 4 * GIB } }]);
  });

  it("reads the new range after a range change", async () => {
    const user = userEvent.setup();
    const { metrics } = renderSection(streams.client);
    const section = screen.getByRole("region", { name: "Apps" });
    await waitFor(() => expect(cardTexts(section)[1]).toBe("RAM | 3 GiB"));

    await user.click(within(screen.getByRole("radiogroup")).getByRole("radio", { name: "6h" }));

    await waitFor(() => expect(streams.watch).toHaveBeenCalledTimes(2));
    expect(metrics.current.range?.label).toBe("6h");
    // The 6h stream answers a different RAM total.
    await waitFor(() => expect(cardTexts(section)[1]).toBe("RAM | 6 GiB"));
  });

  it("says app metrics could not be read, keeping the count card", async () => {
    const client = streams.client;
    client.metrics.listMetricDescriptors = () => Promise.reject(new Error("down"));
    renderSection(client);

    expect(await screen.findByText("App metrics could not be read.")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^CPU/ })).toBeNull();
    expect(screen.getByRole("link", { name: "Apps" })).toBeInTheDocument();
  });
});

function renderSection(client: MetricsClient): { metrics: { current: MetricCards } } {
  const metrics = {} as { current: MetricCards };
  const specs = overviewCardSpecs(facts);
  const splits = overviewSplits([], [], [], apps);
  const watch = new FakeResourceWatch({
    sources: {
      applicationDeployments: () =>
        Promise.resolve(create(ListApplicationDeploymentsResponseSchema, { deployments: apps })),
    },
  });
  function Page() {
    metrics.current = useMetricCards(client, specs, { initialSplit: overviewSplitIds.app, splits });
    return (
      <>
        <MetricRangeControl label="Overview range" metrics={metrics.current} />
        <OverviewAppsSection metrics={metrics.current} />
      </>
    );
  }
  act(() => {
    render(
      <MemoryRouter>
        <ResourceWatchProvider client={watch.client()} reopenDelayMs={0}>
          <Page />
        </ResourceWatchProvider>
      </MemoryRouter>,
    );
  });

  return { metrics };
}

/**
 * A metrics client answering the apps' gauges: the first stream (15m) and the second (6h) differ in
 * their RAM, so a range change shows. Counts the streams it opens.
 */
function countedWatch() {
  let opened = 0;
  const watch = vi.fn<MetricsClient["metrics"]["watchMetrics"]>((request, options) => {
    opened += 1;
    const scale = opened;
    return staticWatch((query) => queryResult({ series: appSeries(query, scale), stepCount: 60 }))(
      request,
      options,
    );
  });

  return {
    client: metricsClientWith(watch),
    queries: (call: number) =>
      (watch.mock.calls[call][0].queries ?? []).map(({ query }) => [query?.metric, query?.groupBy]),
    watch,
  };
}

/** Chat on node-a and node-b, Notes on node-a, and an app on a node without a CPU count. */
function appSeries(query: MessageInitShape<typeof MetricQuerySchema> | undefined, scale: number) {
  const gauge = (labels: Record<string, string>, value: number) =>
    metricSeries(labels, [gaugePoint(0, value)]);

  switch (query?.metric) {
    case "application.host.cpu.utilization":
      return [
        gauge({ deployment: "dep-a", node: "node-a" }, 0.5),
        gauge({ deployment: "dep-a", node: "node-b" }, 0.25),
        gauge({ deployment: "dep-b", node: "node-a" }, 0.25),
        gauge({ deployment: "dep-c", node: "node-c" }, 0.9),
      ];
    case "application.host.memory.used":
      return [
        gauge({ deployment: "dep-a" }, 2 * GIB * scale),
        gauge({ deployment: "dep-b" }, GIB * scale),
      ];
    case "application.accelerator.memory.used":
      return [gauge({ deployment: "dep-a" }, 4 * GIB)];
    default:
      return [];
  }
}

/** Each card as "label | value unit | line" from its rendered text. */
function cardTexts(section: HTMLElement): string[] {
  return within(section)
    .getAllByRole("button", { pressed: false })
    .map((card) => textOf(card));
}

function textOf(card: Element): string {
  return [
    card.querySelector('[data-slot="metric-card-label"]'),
    card.querySelector('[data-slot="metric-card-value"]'),
    card.querySelector('[data-slot="metric-card-line"]'),
  ]
    .map((part) => part?.textContent?.replace(/\s+/g, " ").trim())
    .filter(Boolean)
    .join(" | ");
}

function chartSeries(cards: MetricCards) {
  return cards.chart?.series.map((series) => ({
    label: series.label,
    values: Object.fromEntries(
      series.values.flatMap((value, index) => (value === null ? [] : [[index, value]])),
    ),
  }));
}
