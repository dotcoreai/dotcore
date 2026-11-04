import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { EmptySchema } from "@bufbuild/protobuf/wkt";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type uPlot from "uplot";
import { beforeEach, describe, expect, it, vi } from "vitest";

import type { MetricsClient } from "@/api/metrics-client";
import { projectSiteMemory } from "@/features/memory/node-memory";
import {
  devicePool,
  estimated,
  hostPool,
  measured,
  nodeMemory,
  other,
  unsized,
} from "@/features/memory/node-memory-test-support";
import type { SiteMemoryState } from "@/features/memory/use-site-memory";
import {
  counterPoint,
  distributionPoint,
  gaugePoint,
  gridStartSeconds,
  idleMetricsClient,
  metricsClientWith,
  metricSeries,
  queryResult,
  staticWatch,
} from "@/features/metrics/metric-test-support";
import { NodeMemoryState } from "@/gen/dotcore/capacity/v1/memory_pb";
import {
  MetricAggregate,
  type MetricQuerySchema,
  MetricReduce,
} from "@/gen/dotcore/metrics/v1/metric_pb";
import { type ModelDeployment, ModelDeploymentSchema } from "@/gen/dotcore/model/v1/deployment_pb";
import { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";

import {
  DeploymentMemoryRows,
  DeploymentMetricsProvider,
  DeploymentRequests,
} from "./deployment-metrics";

const plots = vi.hoisted(() => ({ created: [] as { data: unknown[][]; opts: uPlot.Options }[] }));

vi.mock("uplot", () => ({
  default: class {
    constructor(opts: uPlot.Options, data: unknown[][]) {
      plots.created.push({ data, opts });
    }
    setData = vi.fn();
    redraw = vi.fn();
    setSize = vi.fn();
    destroy = vi.fn();
  },
}));

const gib = 1024n ** 3n;

beforeEach(() => {
  plots.created.length = 0;
});

describe("DeploymentMemoryRows", () => {
  it("reads out a dedicated-machine deployment's own VRAM and names why host RAM has no figure", async () => {
    const user = userEvent.setup();
    const gemma = deployment("gemma", "gpu1", { deviceId: "GPU-0", bytes: 10n * gib });
    const qwen = deployment("qwen", "gpu1", { deviceId: "GPU-0", bytes: 17n * gib });
    renderMemory(
      gemma,
      ready([gemma, qwen], {
        gpu1: [measured("gemma", 10n * gib), measured("qwen", 17n * gib), other(gib)],
      }),
    );

    expect(screen.getByRole("button", { name: "VRAM 10 / 32 GiB" })).toBeVisible();
    const ram = screen.getByRole("button", {
      name: "RAM — / 62 GiB, this model: host memory isn't estimated or measured",
    });
    expect(ram).toHaveTextContent("— / 62 GiB");
    const [vram] = screen.getAllByRole("img");
    expect([...vram.children].map((segment) => segment.className.includes("grayscale"))).toEqual([
      false,
      true,
      false,
    ]);

    await user.hover(ram);
    expect(
      await screen.findByText("This model: host memory isn't estimated or measured"),
    ).toBeVisible();
  });

  it("reads out a dedicated-machine deployment's host memory estimate on its RAM row", async () => {
    const user = userEvent.setup();
    const ninfer = deployment(
      "ninfer",
      "gpu1",
      { deviceId: "GPU-0", bytes: 26n * gib },
      { hostEstimate: 6n * gib },
    );
    const gemma = deployment(
      "gemma",
      "gpu1",
      { deviceId: "GPU-0", bytes: 2n * gib },
      { hostEstimate: gib },
    );
    renderMemory(
      ninfer,
      ready([ninfer, gemma], {
        gpu1: [measured("ninfer", 26n * gib), measured("gemma", 2n * gib)],
        gpu1Host: [estimated("ninfer", 6n * gib), estimated("gemma", gib), other(2n * gib)],
      }),
    );

    const ramReadout = screen.getByRole("button", { name: "RAM 6 / 62 GiB" });
    const [, ramBar] = screen.getAllByRole("img");
    expect(
      [...ramBar.children].map((segment) => [
        segment.getAttribute("data-deployment-id"),
        segment.className.includes("grayscale"),
      ]),
    ).toEqual([
      ["ninfer", false],
      ["gemma", true],
      [null, false],
    ]);

    await user.hover(ramReadout);
    expect(await screen.findByText("This model: 6 GiB (estimated)")).toBeVisible();
  });

  it("shows a unified-machine deployment in its single RAM row", () => {
    const qwen = deployment("qwen", "spark", { deviceId: "GB10", bytes: 46n * gib });
    renderMemory(qwen, ready([qwen], { spark: [measured("qwen", 46n * gib), other(22n * gib)] }));

    expect(screen.getAllByRole("img")).toHaveLength(1);
    expect(screen.getByRole("button", { name: "RAM 46 / 128 GiB" })).toBeVisible();
  });

  it("names why the deployment's usage isn't measured", () => {
    const simulated = deployment("simulated", "spark");
    renderMemory(simulated, ready([simulated], { sparkUnsized: ["simulated"] }));

    expect(screen.getByRole("img")).toHaveAttribute(
      "aria-label",
      "RAM on spark: 68 GiB used of 128 GiB; not measured: simulated (no catalog sizing)",
    );
    expect(
      screen.getByRole("button", {
        name: "RAM — / 128 GiB, this model: not measured, no catalog sizing",
      }),
    ).toBeVisible();
  });

  it("holds an empty RAM row while memory loads, unless the deployment is not placed", () => {
    const placed = deployment("gemma", "gpu1");
    const { unmount } = renderMemory(placed, { kind: "loading" });
    expect(rowsText(document.body)).toEqual([["RAM", "—"]]);
    expect(screen.queryByText("Loading")).not.toBeInTheDocument();
    unmount();

    const unplaced = create(ModelDeploymentSchema, {
      modelDeploymentId: "queued",
      snapshot: { source: { case: "local", value: { modelDisplayName: "queued", modelId: "q" } } },
      state: { status: ModelDeploymentStatus.QUEUED },
    });
    renderMemory(unplaced, { kind: "loading" });
    expect(screen.getByText("Not placed")).toBeVisible();
    expect(rowsText(document.body)).toEqual([]);
  });

  it("states a status instead of rows without a placement or a readable machine", () => {
    const unplaced = create(ModelDeploymentSchema, {
      modelDeploymentId: "queued",
      snapshot: { source: { case: "local", value: { modelDisplayName: "queued", modelId: "q" } } },
      state: { status: ModelDeploymentStatus.QUEUED },
    });
    const { unmount } = renderMemory(unplaced, ready([unplaced]));
    expect(screen.getByText("Not placed")).toBeVisible();
    expect(screen.queryByRole("img")).not.toBeInTheDocument();
    unmount();

    const elsewhere = deployment("elsewhere", "unreadable");
    renderMemory(elsewhere, ready([elsewhere]));
    expect(screen.getByText("Unavailable")).toBeVisible();
    expect(screen.queryByRole("img")).not.toBeInTheDocument();
  });

  it("expands the VRAM row to the deployment's summed memory against capacity", async () => {
    const user = userEvent.setup();
    const watchMetrics = vi.fn(
      staticWatch(() =>
        queryResult({
          series: [
            metricSeries({}, [gaugePoint(0, 2 * Number(gib)), gaugePoint(3, 4 * Number(gib))]),
          ],
          stepCount: 4,
        }),
      ),
    );
    const gemma = deployment("gemma", "gpu1", { deviceId: "GPU-0", bytes: 10n * gib });
    renderMemory(
      gemma,
      ready([gemma], { gpu1: [measured("gemma", 10n * gib), other(18n * gib)] }),
      metricsClientWith(watchMetrics),
    );

    // Collapsed, only the cards' stream opens.
    await waitFor(() => expect(watchMetrics).toHaveBeenCalledTimes(1));
    expect(historyQueries(watchMetrics)).toEqual([]);
    await user.click(screen.getByRole("button", { expanded: false, name: "VRAM" }));

    expect(
      await screen.findByRole("img", { name: "Deployment memory history plot" }),
    ).toBeVisible();
    expect(screen.queryByText("VRAM: dedicated GPU memory")).not.toBeInTheDocument();
    const history = historyQueries(watchMetrics);
    expect(history[0]).toMatchObject({
      aggregate: MetricAggregate.SUM,
      groupBy: [],
      matchers: { deployment: "gemma" },
      metric: "deployment.accelerator.memory.used",
      reduce: MetricReduce.LAST,
    });
    expect(plots.created.at(-1)?.data).toEqual([
      [0, 1, 2, 3].map((offset) => gridStartSeconds + offset),
      [2 * Number(gib), null, null, 4 * Number(gib)],
      Array.from({ length: 4 }, () => 32 * Number(gib)),
      [null, null, null, null],
      [null, null, null, null],
    ]);
  });

  it("doesn't expand a dedicated machine's RAM row, which has no deployment history", async () => {
    const user = userEvent.setup();
    const gemma = deployment("gemma", "gpu1", { deviceId: "GPU-0", bytes: 10n * gib });
    renderMemory(gemma, ready([gemma]), metricsClientWith(staticWatch(emptyAnswer)));

    expect(screen.queryByRole("button", { name: "RAM" })).not.toBeInTheDocument();
    await user.click(screen.getAllByRole("img")[1]);

    expect(screen.queryByRole("radiogroup", { name: "History range" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { expanded: false, name: "VRAM" })).toBeVisible();
  });

  it("states that history could not be read when the store's ranges fail", async () => {
    const user = userEvent.setup();
    const gemma = deployment("gemma", "gpu1", { deviceId: "GPU-0", bytes: 10n * gib });
    renderMemory(gemma, ready([gemma]), {
      metrics: {
        listMetricDescriptors: () => Promise.reject(new Error("unavailable")),
        watchMetrics: staticWatch(emptyAnswer),
      },
    });

    await user.click(screen.getByRole("button", { expanded: false, name: "VRAM" }));

    expect(await screen.findByText("History could not be read.")).toBeVisible();
  });
});

describe("DeploymentRequests", () => {
  it("shows six cards over the range, charting one on a click and hiding it on the next", async () => {
    const user = userEvent.setup();
    renderRequests(metricsClientWith(staticWatch(requestAnswer({ active: true }))));

    const section = screen.getByRole("region", { name: "Requests" });
    await waitFor(() =>
      expect(within(section).getByRole("button", { name: /^Requests/ })).toHaveTextContent("3"),
    );
    expect(cardValues(section)).toEqual([
      ["Requests", "3"],
      ["Processed tokens", "50"],
      ["Generated tokens", "20"],
      ["Prompt processing", "900 tokens/s"],
      ["Time to first token", "250 ms"],
      ["Token generation", "42 tokens/s"],
    ]);
    expect(within(section).queryByRole("img")).not.toBeInTheDocument();

    await user.click(within(section).getByRole("button", { name: /^Time to first token/ }));
    expect(
      await within(section).findByRole("img", { name: "Time to first token history plot" }),
    ).toBeVisible();
    expect(within(section).getByText("p95")).toBeVisible();

    await user.click(within(section).getByRole("button", { name: /^Time to first token/ }));
    expect(within(section).queryByRole("img")).not.toBeInTheDocument();
  });

  it("keeps the cards of an idle deployment, with zero counts and dashes for speeds", async () => {
    renderRequests(metricsClientWith(staticWatch(requestAnswer({ active: false }))));

    const section = screen.getByRole("region", { name: "Requests" });
    await waitFor(() =>
      expect(within(section).getByRole("button", { name: /^Requests/ })).toHaveTextContent("0"),
    );
    expect(cardValues(section)).toEqual([
      ["Requests", "0"],
      ["Processed tokens", "0"],
      ["Generated tokens", "0"],
      ["Prompt processing", "—"],
      ["Time to first token", "—"],
      ["Token generation", "—"],
    ]);
  });

  it("holds at most a cards and a memory history stream with a card selected and the memory row expanded", async () => {
    const user = userEvent.setup();
    const watchMetrics = vi.fn(staticWatch(requestAnswer({ active: true })));
    const gemma = deployment("gemma", "gpu1", { deviceId: "GPU-0", bytes: 10n * gib });
    render(
      <DeploymentMetricsProvider active client={metricsClientWith(watchMetrics)} deployment={gemma}>
        <DeploymentMemoryRows deployment={gemma} state={ready([gemma])} />
        <DeploymentRequests />
      </DeploymentMetricsProvider>,
    );

    await user.click(screen.getByRole("button", { expanded: false, name: "VRAM" }));
    await user.click(await screen.findByRole("button", { name: /^Time to first token/ }));

    await screen.findByRole("img", { name: "Time to first token history plot" });
    await screen.findByRole("img", { name: "Deployment memory history plot" });
    expect(openStreams(watchMetrics)).toBe(2);
  });

  it("charts the memory history over the range picked for the cards, with no picker of its own", async () => {
    const user = userEvent.setup();
    const watchMetrics = vi.fn(staticWatch(requestAnswer({ active: true })));
    const gemma = deployment("gemma", "gpu1", { deviceId: "GPU-0", bytes: 10n * gib });
    render(
      <DeploymentMetricsProvider active client={metricsClientWith(watchMetrics)} deployment={gemma}>
        <DeploymentMemoryRows deployment={gemma} state={ready([gemma])} />
        <DeploymentRequests />
      </DeploymentMetricsProvider>,
    );
    await user.click(screen.getByRole("button", { expanded: false, name: "VRAM" }));
    await screen.findByRole("img", { name: "Deployment memory history plot" });
    expect(screen.getAllByRole("radiogroup")).toHaveLength(1);

    await user.click(screen.getByRole("radio", { name: "6h" }));

    await waitFor(() => {
      expect(historyQueries(watchMetrics).at(-1)?.range).toMatchObject({
        case: "last",
        value: { seconds: 21_600n },
      });
    });
  });

  it("keeps what it showed while inactive, with no stream open, and when active again", async () => {
    const watchMetrics = vi.fn(staticWatch(requestAnswer({ active: true })));
    const client = metricsClientWith(watchMetrics);
    const dep = deployment("dep-1", "gpu1");
    const provider = (active: boolean) => (
      <DeploymentMetricsProvider active={active} client={client} deployment={dep}>
        <DeploymentRequests />
      </DeploymentMetricsProvider>
    );
    const { rerender } = render(provider(true));
    const section = screen.getByRole("region", { name: "Requests" });
    await waitFor(() => expect(cardValues(section)[4]).toEqual(["Time to first token", "250 ms"]));

    rerender(provider(false));
    expect(openStreams(watchMetrics)).toBe(0);
    expect(cardValues(section)[4]).toEqual(["Time to first token", "250 ms"]);

    rerender(provider(true));
    expect(cardValues(section)[4]).toEqual(["Time to first token", "250 ms"]);
    await waitFor(() => expect(openStreams(watchMetrics)).toBe(1));
  });
});

/** Each metric row as its label and value. */
function rowsText(container: HTMLElement): string[][] {
  return [...container.querySelectorAll('[data-slot="metric-row"]')].map((row) => [
    row.querySelector('[data-slot="metric-row-toggle"], [data-slot="metric-row-label"]')
      ?.textContent ?? "",
    row.lastElementChild?.textContent ?? "",
  ]);
}

/** Each request card as its label and value with unit. */
function cardValues(section: HTMLElement): string[][] {
  return [...section.querySelectorAll('[data-slot="metric-card"]')].map((card) => [
    card.querySelector('[data-slot="metric-card-label"]')?.textContent ?? "",
    card.querySelector('[data-slot="metric-card-value"]')?.textContent ?? "",
  ]);
}

/** The watch streams still open: those whose abort signal has not fired. */
function openStreams(watchMetrics: ReturnType<typeof vi.fn>) {
  return watchMetrics.mock.calls.filter(([, options]) => {
    const signal = (options as { signal?: AbortSignal } | undefined)?.signal;
    return !signal?.aborted;
  }).length;
}

function historyQueries(
  watchMetrics: ReturnType<typeof vi.fn<MetricsClient["metrics"]["watchMetrics"]>>,
) {
  return watchMetrics.mock.calls
    .flatMap(([request]) => request.queries ?? [])
    .map(({ query }) => query)
    .filter((query) => query?.metric === "deployment.accelerator.memory.used");
}

type Query = MessageInitShape<typeof MetricQuerySchema> | undefined;

function emptyAnswer() {
  return queryResult({ series: [], stepCount: 1 });
}

/** Answers over 60 steps, with traffic in the first step of the range unless idle. */
function requestAnswer({ active }: { active: boolean }) {
  const counter = (delta: number) =>
    active ? [metricSeries({}, [counterPoint(0, 0, false, delta)])] : [];
  const distribution = (count: number, sum: number) =>
    active ? [metricSeries({}, [distributionPoint(0, { 0.95: 1.5 }, false, { count, sum })])] : [];
  const answers: Record<string, ReturnType<typeof metricSeries>[]> = {
    "router.attempt.decode_rate": distribution(2, 84),
    "router.attempt.prefill_rate": distribution(2, 1800),
    "router.attempt.ttft": distribution(2, 0.5),
    "router.attempts": counter(3),
    "router.tokens.cached_prompt": counter(10),
    "router.tokens.completion": counter(20),
    "router.tokens.prompt": counter(50),
  };

  return (query: Query) =>
    queryResult({
      series: answers[query?.metric ?? ""] ?? [],
      stepCount: 60,
      stepSeconds: 15,
    });
}

function renderMemory(
  deployment: ModelDeployment,
  state: SiteMemoryState,
  client: MetricsClient = idleMetricsClient(),
) {
  return render(
    <DeploymentMetricsProvider active client={client} deployment={deployment}>
      <DeploymentMemoryRows deployment={deployment} state={state} />
    </DeploymentMetricsProvider>,
  );
}

function renderRequests(client: MetricsClient) {
  return render(
    <DeploymentMetricsProvider active client={client} deployment={deployment("dep-1", "gpu1")}>
      <DeploymentRequests />
    </DeploymentMetricsProvider>,
  );
}

/**
 * A site with a dedicated gpu1 (a 32 GiB GPU and 62 GiB of host memory), a unified spark (128 GiB)
 * and a machine without a memory reading, each pool holding the given segments; by default 28 GiB
 * of other use on the GPU, 9 GiB on gpu1's host and 68 GiB on spark.
 */
function ready(
  deployments: ModelDeployment[],
  {
    gpu1 = [other(28n * gib)],
    gpu1Host = [other(9n * gib)],
    spark = [other(68n * gib)],
    sparkUnsized = [],
  }: {
    gpu1?: Parameters<typeof devicePool>[1];
    gpu1Host?: Parameters<typeof hostPool>[1];
    spark?: Parameters<typeof hostPool>[1];
    sparkUnsized?: string[];
  } = {},
): SiteMemoryState {
  const memory = projectSiteMemory({
    deployments,
    machines: ["gpu1", "spark", "unreadable"].map((name) => ({ machineId: name, name })),
    nodes: [
      nodeMemory("gpu1", [
        devicePool(32n * gib, gpu1, { deviceId: "GPU-0" }),
        hostPool(62n * gib, gpu1Host),
      ]),
      nodeMemory("spark", [
        hostPool(
          128n * gib,
          spark,
          {},
          sparkUnsized.map((id) => unsized({ modelDeploymentId: id })),
        ),
      ]),
      nodeMemory("unreadable", [], NodeMemoryState.UNAVAILABLE),
    ],
    readFailed: false,
  });

  return { kind: "ready", memory };
}

function deployment(
  id: string,
  machineId: string,
  row?: { bytes: bigint; deviceId: string },
  { hostEstimate }: { hostEstimate?: bigint } = {},
): ModelDeployment {
  return create(ModelDeploymentSchema, {
    acceleratorMemory: {
      result: row
        ? {
            case: "available",
            value: {
              devices: [
                { deviceId: row.deviceId, managedNodeId: machineId, usedMemoryBytes: row.bytes },
              ],
            },
          }
        : { case: "notApplicable", value: create(EmptySchema) },
    },
    modelDeploymentId: id,
    snapshot: {
      source: {
        case: "local",
        value: {
          modelDisplayName: id,
          modelId: id,
          sizing: hostEstimate === undefined ? undefined : { hostMemoryBytes: hostEstimate },
        },
      },
    },
    state: {
      observedPlacements: [{ managedNodeId: machineId }],
      status: ModelDeploymentStatus.READY,
    },
  });
}
