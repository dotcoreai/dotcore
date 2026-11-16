import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { act, render, screen, waitFor, within } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { ApplicationClient } from "@/api/application-client";
import type { MetricsClient } from "@/api/metrics-client";
import type { ModelClient } from "@/api/model-client";
import {
  gb10Snapshot,
  idleNodeMemory,
  listedMachine,
  macSnapshot,
  workstationSnapshot,
} from "@/features/memory/machine-test-support";
import {
  gaugePoint,
  metricsClientWith,
  metricSeries,
  queryResult,
  staticWatch,
} from "@/features/metrics/metric-test-support";
import { ListApplicationDeploymentsResponseSchema } from "@/gen/dotcore/application/v1/service_pb";
import { ListNodeMemoryResponseSchema } from "@/gen/dotcore/capacity/v1/service_pb";
import type { MetricQuerySchema } from "@/gen/dotcore/metrics/v1/metric_pb";
import { type ModelDeployment, ModelDeploymentSchema } from "@/gen/dotcore/model/v1/deployment_pb";
import { ListModelDeploymentsResponseSchema } from "@/gen/dotcore/model/v1/service_pb";
import type { HardwareSnapshot } from "@/gen/dotcore/observation/v1/observation_pb";
import {
  ListConfiguredExecutionTargetsResponseSchema,
  ListMachinesResponseSchema,
  type Machine,
  MachineHardwareSnapshotSchema,
  NodeObservationState,
} from "@/gen/dotcore/site/v1/service_pb";
import { hardwareSnapshotsPayload } from "@/test/fake-resource-watch";
import { rejectConsoleSettingsUpdate } from "@/test/fake-site-client";
import { expectResourceMarker } from "@/test/resource-marker";
import { setupUser } from "@/test/user";
import { getResourceColor } from "@/theme/resource-color";

import { MachinesPage, type MachinesSite } from "./machines-page-test-support";

vi.mock("uplot", () => ({
  default: class {
    setData = vi.fn();
    redraw = vi.fn();
    setSize = vi.fn();
    destroy = vi.fn();
  },
}));

afterEach(() => {
  window.localStorage.clear();
});

const gib = 1024 ** 3;

describe("MachinesResources", () => {
  it("combines the connected machines' resources in rows, read on one current stream", async () => {
    const watchMetrics = vi.fn(staticWatch(answer()));
    renderPage(metricsClientWith(watchMetrics));

    const resources = await screen.findByRole("region", { name: "Resources" });
    await waitFor(() =>
      expect(rows(resources)).toEqual([
        ["CPU", "25% · 52 CPUs"],
        ["GPU", "38% · 130 W"],
        ["RAM", "103 / 181 GiB"],
        ["VRAM", "0.5 / 32 GiB"],
        ["Storage", "1.3 TiB / 6.4 TiB"],
      ]),
    );
    expect(within(resources).queryByText(/Not counted/)).not.toBeInTheDocument();

    // The resources stream, and the machines table's one-second stream of its own.
    expect(watchMetrics).toHaveBeenCalledTimes(2);
    const [resourcesRequest] = watchMetrics.mock.calls
      .map(([request]) => request)
      .sort((a, b) => (a.queries?.length ?? 0) - (b.queries?.length ?? 0));
    const queries = (resourcesRequest.queries ?? []).map(({ query }) => query);
    expect(queries.map((query) => [query?.metric, query?.groupBy])).toEqual([
      ["node.host.cpu.utilization", ["node"]],
      ["node.filesystem.used", ["node"]],
      ["node.accelerator.utilization", ["node", "device"]],
      ["node.accelerator.power", ["node", "device"]],
    ]);
    expect(queries.every((query) => Object.keys(query?.matchers ?? {}).length === 0)).toBe(true);
  });

  it("holds every row empty and expandable while the machines and hardware are read", async () => {
    let releaseMachines!: () => void;
    let readSnapshots!: () => void;
    const listed = new Promise<void>((resolve) => (releaseMachines = resolve));
    const read = new Promise<void>((resolve) => (readSnapshots = resolve));
    renderPage(metricsClientWith(staticWatch(answer())), undefined, [], listed, read);
    const pending = [
      ["CPU", "—"],
      ["GPU", "—"],
      ["RAM", "—"],
      ["VRAM", "—"],
      ["Storage", "—"],
    ];

    const resources = await screen.findByRole("region", { name: "Resources" });
    expect(rows(resources)).toEqual(pending);
    for (const [label] of pending) {
      expect(within(resources).getByRole("button", { name: label })).toHaveAttribute(
        "aria-expanded",
        "false",
      );
    }

    releaseMachines();
    await act(async () => {
      await Promise.resolve();
    });
    expect(rows(resources)).toEqual(pending);

    readSnapshots();
    await waitFor(() =>
      expect(rows(resources)).toEqual([
        ["CPU", "25% · 52 CPUs"],
        ["GPU", "38% · 130 W"],
        ["RAM", "103 / 181 GiB"],
        ["VRAM", "0.5 / 32 GiB"],
        ["Storage", "1.3 TiB / 6.4 TiB"],
      ]),
    );
  });

  it("leaves the section out on a site without machines", async () => {
    renderPage(metricsClientWith(staticWatch(answer())), []);

    expect(await screen.findByText("No machines are configured for this site.")).toBeVisible();
    expect(screen.queryByRole("region", { name: "Resources" })).not.toBeInTheDocument();
  });

  it("keeps each machine's share By machine, named with its marker", async () => {
    const user = setupUser();
    renderPage(metricsClientWith(staticWatch(answer())));
    const resources = await screen.findByRole("region", { name: "Resources" });
    await waitFor(() => expect(rows(resources)).toHaveLength(5));
    // Memory splits by machine in machine colors, in either view.
    await waitFor(() =>
      expect(memoryPartColors(rowElement(resources, "RAM"))).toEqual([
        [cssColor("gpu1"), cssColor("spark")],
      ]),
    );

    await user.click(within(resources).getByRole("button", { name: "By machine" }));

    const cpu = rowElement(resources, "CPU");
    expect(spanNames(cpu)).toEqual(["gpu1 12%", "spark-01 46%"]);
    expectResourceMarker(within(cpu).getByText(/gpu1/), "gpu1");
    expect(fillColors(cpu)).toEqual([cssColor("gpu1"), cssColor("spark")]);
    expect(spanNames(rowElement(resources, "RAM"))).toEqual(["gpu1", "spark-01"]);
    expect(memoryPartColors(rowElement(resources, "RAM"))).toEqual([
      [cssColor("gpu1")],
      [cssColor("spark")],
    ]);
    expect(rows(resources)[0]).toEqual(["CPU", "25% · 52 CPUs"]);
  });

  it("shows a single machine without the view choice, and draws rows it cannot fill empty", async () => {
    const user = setupUser();
    renderPage(metricsClientWith(staticWatch(answer())), [
      [listedMachine("mbp", "mbp"), macSnapshot()],
    ]);

    const resources = await screen.findByRole("region", { name: "Resources" });
    await waitFor(() => expect(rows(resources)[2][1]).not.toBe("—"));
    expect(rows(resources).map(([label]) => label)).toEqual([
      "CPU",
      "GPU",
      "RAM",
      "VRAM",
      "Storage",
    ]);
    expect(rows(resources)[1]).toEqual(["GPU", "—"]);
    expect(rows(resources)[3]).toEqual(["VRAM", "—"]);
    expect(within(resources).queryByRole("group", { name: "Resources view" })).toBeNull();

    // An empty row still expands, to an empty history chart.
    await user.click(within(resources).getByRole("button", { name: "VRAM" }));
    expect(await screen.findByRole("group", { name: "VRAM history" })).toBeVisible();
  });

  it("expands a row to per-machine history on one history stream, closed once collapsed", async () => {
    const user = setupUser();
    const watchMetrics = vi.fn(staticWatch(answer()));
    renderPage(metricsClientWith(watchMetrics));
    const resources = await screen.findByRole("region", { name: "Resources" });
    await waitFor(() => expect(rows(resources)[0]).toEqual(["CPU", "25% · 52 CPUs"]));
    await user.click(within(resources).getByRole("button", { name: "By machine" }));

    await user.click(within(resources).getByRole("button", { name: "CPU" }));
    const chart = await screen.findByRole("group", { name: "CPU history" });
    expect(chartReadout(chart)).toBe("gpu1 12% spark-01 46%");
    expect(swatchColors(chart)).toEqual([cssColor("gpu1"), cssColor("spark")]);
    // The table's stream, the current stream and the history stream.
    expect(watchMetrics).toHaveBeenCalledTimes(3);

    await user.click(within(resources).getByRole("button", { name: "All machines" }));
    expect(chartReadout(screen.getByRole("group", { name: "CPU history" }))).toBe("CPU 25%");
    await user.click(within(resources).getByRole("button", { name: "RAM" }));
    const ram = chartReadout(await screen.findByRole("group", { name: "RAM history" }));
    expect(ram).toBe("Used 103 GiB Capacity 181 GiB");
    expect(watchMetrics).toHaveBeenCalledTimes(3);

    await user.click(within(resources).getByRole("button", { name: "CPU" }));
    await user.click(within(resources).getByRole("button", { name: "RAM" }));
    expect(screen.queryByRole("group", { name: "RAM history" })).toBeNull();
    // Reopening history after every row closed needs a new stream: the last one was closed.
    await user.click(within(resources).getByRole("button", { name: "CPU" }));
    await waitFor(() => expect(watchMetrics).toHaveBeenCalledTimes(4));
  });

  it("draws memory history by machine only, with no line per model", async () => {
    const user = setupUser();
    const watchMetrics = vi.fn(
      staticWatch(
        answer({
          "deployment.accelerator.memory.used": [
            latest({ deployment: "llama", device: "gb10", node: "spark" }, 21 * gib),
            latest({ deployment: "gemma", device: "gpu-0", node: "gpu1" }, 0.25 * gib),
          ],
        }),
      ),
    );
    renderPage(metricsClientWith(watchMetrics), undefined, [
      localDeployment("llama", "Llama 3.1 8B"),
      localDeployment("gemma", "Gemma 3 27B"),
    ]);
    const resources = await screen.findByRole("region", { name: "Resources" });
    await waitFor(() => expect(rows(resources)).toHaveLength(5));

    await user.click(within(resources).getByRole("button", { name: "RAM" }));
    const ram = await screen.findByRole("group", { name: "RAM history" });
    expect(chartReadout(ram)).toBe("Used 103 GiB Capacity 181 GiB");
    await user.click(within(resources).getByRole("button", { name: "VRAM" }));
    const vram = await screen.findByRole("group", { name: "VRAM history" });
    expect(chartReadout(vram)).toBe("Used 512 MiB Capacity 32 GiB");

    await user.click(within(resources).getByRole("button", { name: "By machine" }));
    const byMachine = screen.getByRole("group", { name: "RAM history" });
    expect(chartReadout(byMachine)).toBe("gpu1 15 GiB spark-01 88 GiB");
    expect(swatchColors(byMachine)).toEqual([cssColor("gpu1"), cssColor("spark")]);
  });

  it("names machines not counted now with their state, and leaves them out of every value", async () => {
    renderPage(metricsClientWith(staticWatch(answer())), [
      [listedMachine("gpu1", "gpu1"), workstationSnapshot()],
      [listedMachine("spark", "spark-01", NodeObservationState.STALE), gb10Snapshot()],
      [listedMachine("mbp", "mbp", NodeObservationState.DISCONNECTED), macSnapshot()],
    ]);

    const resources = await screen.findByRole("region", { name: "Resources" });
    await waitFor(() =>
      expect(rows(resources)).toEqual([
        ["CPU", "12% · 32 CPUs"],
        ["GPU", "4% · 38 W"],
        ["RAM", "15 / 62 GiB"],
        ["VRAM", "0.5 / 32 GiB"],
        ["Storage", "900 GiB / 5.5 TiB"],
      ]),
    );
    expect(
      within(resources).getByText("Not counted: spark-01 (stale), mbp (disconnected)"),
    ).toBeVisible();
  });

  it("reads a value missing a machine's reading as partial, naming what it leaves out", async () => {
    renderPage(
      metricsClientWith(
        staticWatch(
          answer({
            // spark's CPU reading is eight seconds old, so it no longer counts as current.
            "node.host.cpu.utilization": [
              latest({ node: "gpu1" }, 0.12),
              metricSeries({ node: "spark" }, [gaugePoint(1, 0.46)]),
              latest({ node: "mbp" }, 0.31),
            ],
          }),
        ),
      ),
      [
        [listedMachine("gpu1", "gpu1"), workstationSnapshot()],
        [listedMachine("spark", "spark-01"), gb10Snapshot()],
        [listedMachine("mbp", "mbp"), macSnapshot()],
      ],
    );

    const resources = await screen.findByRole("region", { name: "Resources" });
    await waitFor(() => expect(rows(resources)).toHaveLength(5));
    const cpu = rowElement(resources, "CPU");
    // (12% × 32 + 31% × 16) / 48
    await waitFor(() => expect(rows(resources)[0]).toEqual(["CPU", "18% · 48 CPUs"]));
    expect(cpu.querySelector('[data-partial="true"]')).not.toBeNull();
    expect(within(cpu).getByRole("button", { name: /18%/ })).toHaveAccessibleName(
      "18% · 48 CPUs; No current reading: spark-01",
    );
    // An unsupported GPU is named, but the value is whole: nothing it could read is missing.
    const gpu = rowElement(resources, "GPU");
    expect(within(gpu).getByRole("button", { name: /38%/ })).toHaveAccessibleName(
      "38% · 130 W; Not supported: mbp",
    );
    expect(gpu.querySelector('[data-partial="true"]')).toBeNull();
  });
});

describe("machine rows", () => {
  it("describes each row by its hardware, and a machine not counted by its state", async () => {
    renderPage(metricsClientWith(staticWatch(answer())), [
      [listedMachine("gpu1", "gpu1"), workstationSnapshot()],
      [listedMachine("spark", "spark-01", NodeObservationState.STALE), gb10Snapshot()],
    ]);

    const gpu1 = await screen.findByRole("row", { name: "gpu1" });
    await waitFor(() =>
      expect(gpu1.querySelector("td")).toHaveTextContent("NVIDIA RTX 5090 · 32 CPUs"),
    );
    expect(gpu1.querySelector('[data-slot="badge"]')).toHaveTextContent("Connected");

    const spark = screen.getByRole("row", { name: "spark-01" });
    expect(spark.querySelector('[data-slot="badge"]')).toHaveTextContent("Stale");
    expect(spark.querySelector("td")).toHaveTextContent("Hardware unavailable");
  });
});

type Query = MessageInitShape<typeof MetricQuerySchema> | undefined;

/** A reading at the newest closed second and the open one after it. */
function latest(labels: Record<string, string>, value: number) {
  return metricSeries(labels, [gaugePoint(8, value), gaugePoint(9, value, true)]);
}

/** Answers each metric with gpu1's and spark's readings, whatever the range. */
function answer(overrides: Record<string, ReturnType<typeof metricSeries>[]> = {}) {
  const byMetric: Record<string, ReturnType<typeof metricSeries>[]> = {
    "node.accelerator.memory.total": [latest({ device: "gpu-0", node: "gpu1" }, 32 * gib)],
    "node.accelerator.memory.used": [latest({ device: "gpu-0", node: "gpu1" }, 0.5 * gib)],
    "node.accelerator.power": [
      latest({ device: "gpu-0", node: "gpu1" }, 38),
      latest({ device: "gb10", node: "spark" }, 92),
    ],
    "node.accelerator.utilization": [
      latest({ device: "gpu-0", node: "gpu1" }, 0.04),
      latest({ device: "gb10", node: "spark" }, 0.71),
    ],
    "node.filesystem.size": [
      latest({ node: "gpu1" }, (1863 + 3726) * gib),
      latest({ node: "spark" }, 1000 * gib),
    ],
    "node.filesystem.used": [
      latest({ node: "gpu1" }, 900 * gib),
      latest({ node: "spark" }, 400 * gib),
    ],
    "node.host.cpu.utilization": [latest({ node: "gpu1" }, 0.12), latest({ node: "spark" }, 0.46)],
    "node.host.memory.available": [
      latest({ node: "gpu1" }, 47 * gib),
      latest({ node: "spark" }, 31 * gib),
    ],
    "node.host.memory.total": [
      latest({ node: "gpu1" }, 62 * gib),
      latest({ node: "spark" }, 119 * gib),
    ],
    ...overrides,
  };

  return (query: Query) =>
    queryResult({ series: byMetric[query?.metric ?? ""] ?? [], stepCount: 10 });
}

/** A local deployment of the catalog model `model-<id>`, named for its card. */
function localDeployment(id: string, name: string): ModelDeployment {
  return create(ModelDeploymentSchema, {
    modelDeploymentId: id,
    modelDeploymentName: name,
    snapshot: { source: { case: "local", value: { modelId: `model-${id}` } } },
  });
}

function renderPage(
  metricsClient: MetricsClient,
  machines: [Machine, HardwareSnapshot][] = [
    [listedMachine("gpu1", "gpu1"), workstationSnapshot()],
    [listedMachine("spark", "spark-01"), gb10Snapshot()],
  ],
  deployments: ModelDeployment[] = [],
  listed: Promise<void> = Promise.resolve(),
  read: Promise<void> = Promise.resolve(),
) {
  const client: MachinesSite = {
    updateConsoleSettings: rejectConsoleSettingsUpdate,
    listConfiguredExecutionTargets: vi.fn(() =>
      Promise.resolve(create(ListConfiguredExecutionTargetsResponseSchema)),
    ),
    watched: {
      hardwareSnapshots: () =>
        read.then(() =>
          hardwareSnapshotsPayload(
            Object.fromEntries(
              machines.map(([machine, snapshot]) => [
                machine.machineId,
                create(MachineHardwareSnapshotSchema, { snapshot }),
              ]),
            ),
          ),
        ),
      machines: () =>
        listed.then(() =>
          create(ListMachinesResponseSchema, { machines: machines.map(([machine]) => machine) }),
        ),
      nodeMemory: () =>
        read.then(() =>
          create(ListNodeMemoryResponseSchema, {
            nodes: machines.map(([machine, snapshot]) =>
              idleNodeMemory(machine.machineId, snapshot),
            ),
          }),
        ),
    },
  };
  const modelClient = {
    catalog: {},
    model: {
      listModelDeployments: vi.fn(() =>
        Promise.resolve(create(ListModelDeploymentsResponseSchema, { deployments })),
      ),
    },
  } as unknown as ModelClient;
  const applicationClient = {
    application: {
      listApplicationDeployments: vi.fn(() =>
        Promise.resolve(create(ListApplicationDeploymentsResponseSchema)),
      ),
    },
  } as unknown as ApplicationClient;

  render(
    <MemoryRouter initialEntries={["/machines"]}>
      <MachinesPage
        applicationClient={applicationClient}
        client={client}
        metricsClient={metricsClient}
        modelClient={modelClient}
      />
    </MemoryRouter>,
  );
}

/** Each metric row as its label and value. */
function rows(container: HTMLElement): string[][] {
  return [...container.querySelectorAll('[data-slot="metric-row"]')].map((row) => [
    row.querySelector('[data-slot="metric-row-toggle"], [data-slot="metric-row-label"]')
      ?.textContent ?? "",
    row.lastElementChild?.textContent ?? "",
  ]);
}

function rowElement(container: HTMLElement, label: string): HTMLElement {
  const row = [...container.querySelectorAll<HTMLElement>('[data-slot="metric-row"]')].find(
    (element) => element.querySelector('[data-slot="metric-row-toggle"]')?.textContent === label,
  );
  if (!row) throw new Error(`no ${label} row`);
  return row;
}

/** The names under a split bar's parts, with their values. */
function spanNames(row: HTMLElement): string[] {
  return [...row.querySelectorAll('[data-slot="metric-span"] p')].map(
    (name) => name.textContent ?? "",
  );
}

function fillColors(row: HTMLElement): string[] {
  return [...row.querySelectorAll<HTMLElement>('[data-slot="metric-bar-fill"]')].map(
    (fill) => fill.style.backgroundColor,
  );
}

/** Each memory bar's drawn parts, by fill color. */
function memoryPartColors(row: HTMLElement): string[][] {
  return [...row.querySelectorAll<HTMLElement>('[role="img"]')].map((bar) =>
    [...bar.querySelectorAll<HTMLElement>(":scope > *")].map((part) => part.style.backgroundColor),
  );
}

function swatchColors(chart: HTMLElement): string[] {
  return [...chart.querySelectorAll<HTMLElement>('[data-slot="chart-swatch"]')].map(
    (swatch) => swatch.style.backgroundColor,
  );
}

/** The readout text above a chart's plot: each line's label and value. */
function chartReadout(chart: HTMLElement): string {
  return [...(chart.firstElementChild?.querySelectorAll("span") ?? [])]
    .filter((span) => span.childElementCount === 0)
    .map((span) => span.textContent)
    .filter(Boolean)
    .join(" ");
}

/** The computed form a browser gives a resource's marker color. */
function cssColor(colorKey: string): string {
  const element = document.createElement("span");
  element.style.backgroundColor = getResourceColor(colorKey);
  return element.style.backgroundColor;
}
