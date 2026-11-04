import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createRef } from "react";
import { MemoryRouter, useLocation } from "react-router-dom";
import { describe, expect, it, vi } from "vitest";

import type { MetricsClient } from "@/api/metrics-client";
import {
  gaugePoint,
  metricsClientWith,
  metricSeries,
  queryResult,
  staticWatch,
} from "@/features/metrics/metric-test-support";
import { useMetricCards } from "@/features/metrics/use-metric-cards";
import type { MetricQuerySchema } from "@/gen/dotcore/metrics/v1/metric_pb";
import { MachineSchema, NodeObservationState } from "@/gen/dotcore/site/v1/service_pb";
import { parsePanelStack } from "@/lib/panel-stack";

import type { MachineFacts } from "./machine-site-values";
import { overviewCardSpecs, overviewSplitIds, overviewSplits } from "./overview-card-specs";
import { OverviewMachinesSection } from "./overview-machines-section";
import type { OverviewMetrics } from "./use-overview-metrics";

vi.mock("uplot", () => ({
  default: class {
    static paths = { bars: () => undefined };

    setData = vi.fn();
    redraw = vi.fn();
    setSize = vi.fn();
    destroy = vi.fn();
  },
}));

const GiB = 1024 ** 3;
const machines = [
  create(MachineSchema, {
    displayName: "Spark",
    machineId: "spark",
    observationFreshness: { state: NodeObservationState.CONNECTED },
  }),
  create(MachineSchema, {
    displayName: "Mac",
    machineId: "mac",
    observationFreshness: { state: NodeObservationState.CONNECTED },
  }),
  create(MachineSchema, {
    displayName: "Old box",
    machineId: "old",
    observationFreshness: { state: NodeObservationState.DISCONNECTED },
  }),
];
const facts: MachineFacts = {
  cpus: new Map([
    ["spark", 20],
    ["mac", 10],
  ]),
  dedicated: new Set(),
  gpus: new Map([
    ["spark", 1],
    ["mac", 1],
  ]),
};
const summaries = new Map([
  ["spark", "NVIDIA GB10 · 20 CPUs"],
  ["mac", "Apple M4 · 10 CPUs"],
]);

describe("OverviewMachinesSection", () => {
  it("shows the machine cards with the newest reading and the range line", async () => {
    renderSection(metricsClientWith(staticWatch(answer)));

    const section = screen.getByRole("region", { name: "Machines" });
    await waitFor(() =>
      expect(cardTexts(section)).toContain("CPU usage | 37 % | avg 37% · peak 37%"),
    );
    expect(cardTexts(section)).toEqual([
      "Machines | 2 connected | of 3 listed",
      "CPU usage | 37 % | avg 37% · peak 37%",
      "RAM | 48 / 96 GiB | peak 48 GiB",
      "GPU usage | 20 % | avg 20% · peak 20%",
      "VRAM | — | unified memory, see RAM",
      "GPU power | 150 W | avg 150 W · peak 150 W",
      "GPU temperature | 70 °C | hottest GPU · peak 70 °C",
      "Storage | 1.3 / 2.5 TiB | peak 1.3 TiB",
    ]);
  });

  it("says nothing of unified memory while the machines' hardware is still being read", async () => {
    renderSection(metricsClientWith(staticWatch(answer)), overviewSplitIds.machine, true);

    const section = screen.getByRole("region", { name: "Machines" });
    await waitFor(() =>
      expect(cardTexts(section)).toContain("CPU usage | 37 % | avg 37% · peak 37%"),
    );
    expect(cardTexts(section)).toContain("VRAM | —");
  });

  it("does not make the connected count selectable and charts a selected card by machine", async () => {
    const user = userEvent.setup();
    const { metrics } = renderSection(metricsClientWith(staticWatch(answer)));
    const section = screen.getByRole("region", { name: "Machines" });
    await waitFor(() => expect(metrics.current.cards[0].reading).toBeDefined());

    expect(
      within(section).getByText("Machines", { selector: "span" }).closest("button"),
    ).toBeNull();
    await user.click(within(section).getByRole("button", { name: /^CPU usage/ }));

    expect(screen.getByRole("radio", { name: "By machine" })).toBeChecked();
    expect(metrics.current.chart?.series.map((series) => series.label)).toEqual(["Spark", "Mac"]);

    await user.click(screen.getByRole("radio", { name: "Total" }));
    expect(metrics.current.chart?.series.map((series) => series.label)).toEqual(["CPU"]);
  });

  it("charts a machine card as the whole site when another kind's split is chosen", async () => {
    const user = userEvent.setup();
    const { metrics } = renderSection(
      metricsClientWith(staticWatch(answer)),
      overviewSplitIds.model,
    );
    await waitFor(() => expect(metrics.current.cards[0].reading).toBeDefined());

    await user.click(screen.getByRole("button", { name: /^CPU usage/ }));

    expect(metrics.current.chart?.series.map((series) => series.label)).toEqual(["CPU"]);
  });

  it("lists every machine with its current readings, dashes when disconnected", async () => {
    renderSection(metricsClientWith(staticWatch(answer)));

    const table = await screen.findByRole("table", { name: "Machines" });
    await waitFor(() => expect(rowTexts(table)[0]).not.toContain("— | —"));
    expect(rowTexts(table)).toEqual([
      "Spark | NVIDIA GB10 · 20 CPUs | 50% | 20% | 32 / 64 GiB | — | 512 GiB / 1.5 TiB",
      "Mac | Apple M4 · 10 CPUs | 10% | 20% | 16 / 32 GiB | — | 768 GiB / 1 TiB",
      "Old box | Disconnected | — | — | — | — | —",
    ]);
  });

  it("opens the machine's sidebar from a row and keeps the row for focus return", async () => {
    const user = userEvent.setup();
    const { location, openerRef } = renderSection(metricsClientWith(staticWatch(answer)));

    await user.click(await screen.findByRole("row", { name: "Mac" }));

    expect(parsePanelStack(location.current)).toEqual([
      { page: "machines", resourceId: "mac", screen: "machine" },
    ]);
    expect(openerRef.current).toBe(screen.getByRole("row", { name: "Mac" }));
  });

  it("opens a one-second stream of its own for the newest readings next to the page's", async () => {
    const watchMetrics = vi.fn(staticWatch(answer));
    renderSection(metricsClientWith(watchMetrics));

    await waitFor(() => expect(watchMetrics).toHaveBeenCalledTimes(2));
    const [now, page] = watchMetrics.mock.calls
      .map(([request]) => request.queries ?? [])
      .sort((a, b) => a.length - b.length);
    expect(page.map((entry) => entry.query?.stepHint?.seconds)).not.toContain(1n);
    expect(now.map((entry) => entry.query?.stepHint?.seconds)).toEqual(Array(10).fill(1n));
    expect(now.map((entry) => entry.query?.metric)).toContain("node.accelerator.temperature");
  });

  it("says machine metrics could not be read when no range is served", async () => {
    const client = metricsClientWith(staticWatch(answer));
    client.metrics.listMetricDescriptors = () => Promise.reject(new Error("down"));
    renderSection(client);

    expect(await screen.findByText("Machine metrics could not be read.")).toBeInTheDocument();
  });
});

function renderSection(
  client: MetricsClient,
  initialSplit: string = overviewSplitIds.machine,
  factsPending = false,
) {
  const metrics = {} as { current: OverviewMetrics };
  const location = { current: "" };
  const openerRef = createRef<HTMLElement | null>() as { current: HTMLElement | null };
  const specs = overviewCardSpecs(facts);
  const splits = overviewSplits([], [], machines);
  function Page() {
    const cards = useMetricCards(client, specs, {
      initialSplit,
      splits,
    });
    metrics.current = { ...cards, fleet: { facts, factsPending, machines, summaries } };
    location.current = useLocation().search;
    return (
      <OverviewMachinesSection
        metrics={metrics.current}
        metricsClient={client}
        openerRef={openerRef}
      />
    );
  }
  act(() => {
    render(
      <MemoryRouter>
        <Page />
      </MemoryRouter>,
    );
  });

  return { location, metrics, openerRef };
}

function cardTexts(section: HTMLElement): string[] {
  return Array.from(section.querySelectorAll('[data-slot="metric-card"]')).map((card) =>
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

function rowTexts(table: HTMLElement): string[] {
  return within(table)
    .getAllByRole("row")
    .slice(1)
    .map((row) =>
      Array.from(row.querySelectorAll("td"))
        .map((cell, index) =>
          index === 0
            ? Array.from(cell.querySelectorAll("div"))
                .map((line) => line.textContent)
                .join(" | ")
            : cell.textContent,
        )
        .join(" | "),
    );
}

/** Two connected machines reporting steady readings. */
function answer(query: MessageInitShape<typeof MetricQuerySchema> | undefined) {
  const groupBy = query?.groupBy ?? [];
  const reading = query?.stepHint?.seconds === 1n ? nowReading : rangeReading;
  const perMachine = (spark: number, mac: number) =>
    groupBy.includes("node")
      ? [reading({ node: "spark" }, spark), reading({ node: "mac" }, mac)]
      : [reading({}, spark + mac)];
  const series = (() => {
    switch (query?.metric) {
      case "node.host.cpu.utilization":
        return [reading({ node: "spark" }, 0.5), reading({ node: "mac" }, 0.1)];
      case "node.accelerator.utilization":
        return groupBy.includes("node") ? perMachine(0.2, 0.2) : [reading({}, 0.2)];
      case "node.accelerator.power":
        return perMachine(100, 50);
      case "node.accelerator.temperature":
        return groupBy.includes("node") ? perMachine(70, 60) : [reading({}, 70)];
      case "node.host.memory.total":
        return perMachine(64 * GiB, 32 * GiB);
      case "node.host.memory.available":
        return perMachine(32 * GiB, 16 * GiB);
      case "node.filesystem.used":
        return perMachine(0.5 * 1024 ** 4, 0.75 * 1024 ** 4);
      case "node.filesystem.size":
        return perMachine(1.5 * 1024 ** 4, 1 * 1024 ** 4);
      default:
        return [];
    }
  })();

  return queryResult({ series, stepCount: 10 });
}

/** The page range's steady readings. */
function rangeReading(labels: Record<string, string>, value: number) {
  return metricSeries(
    labels,
    Array.from({ length: 10 }, (_, step) => gaugePoint(step, value)),
  );
}

/** The "now" stream's readings: closed steps of `value`, then an open step far from it. */
function nowReading(labels: Record<string, string>, value: number) {
  return metricSeries(labels, [
    ...Array.from({ length: 9 }, (_, step) => gaugePoint(step, value)),
    gaugePoint(9, value * 10, true),
  ]);
}
