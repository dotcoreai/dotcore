import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import type { MetricsClient } from "@/api/metrics-client";
import { projectNodeMemory } from "@/features/memory/node-memory";
import {
  devicePool,
  hostPool,
  nodeMemory,
  other,
} from "@/features/memory/node-memory-test-support";
import {
  gaugePoint,
  metricsClientWith,
  metricSeries,
  queryResult,
  staticWatch,
} from "@/features/metrics/metric-test-support";
import type { MetricQuerySchema } from "@/gen/dotcore/metrics/v1/metric_pb";
import {
  type AcceleratorFactsSchema,
  AcceleratorMemoryArchitecture,
  AcceleratorSignal,
  AcceleratorVendor,
  type HardwareSnapshot,
  HardwareSnapshotSchema,
} from "@/gen/dotcore/observation/v1/observation_pb";
import { setupUser } from "@/test/user";

import { MachineMetricSections } from "./machine-metrics";

vi.mock("uplot", () => ({
  default: class {
    setData = vi.fn();
    redraw = vi.fn();
    setSize = vi.fn();
    destroy = vi.fn();
  },
}));

const gib = 1024 ** 3;

describe("MachineMetricSections", () => {
  it("reads each device's supported signals, the CPU and each filesystem from the latest second", async () => {
    const watchMetrics = vi.fn(staticWatch(answer()));
    renderSections(metricsClientWith(watchMetrics));

    const fast = screen.getByRole("group", { name: "GPU 0 · NVIDIA RTX 5090" });
    expect(await within(fast).findByText("90%")).toBeVisible();
    expect(rows(fast)).toEqual([
      ["VRAM", "2 / 32 GiB"],
      ["Utilization", "90%"],
      ["Temperature", "61 °C"],
      ["Power", "200 W"],
    ]);
    // Temperature fills against 100 °C, power against its peak of the window.
    expect(fills(fast)).toEqual(["90%", "61%", "50%"]);

    const slow = screen.getByRole("group", { name: "GPU 1 · NVIDIA RTX 4090" });
    expect(rows(slow)).toEqual([["Utilization", "90%"]]);

    expect(rows(screen.getByRole("region", { name: "Resources" }))).toEqual([
      ["RAM", "24 / 64 GiB"],
      ["CPU", "90%"],
    ]);
    const storage = screen.getByRole("region", { name: "Storage" });
    expect(rows(storage)).toEqual([["/", "256 GiB / 1 TiB"]]);
    expect(fills(storage)).toEqual(["25%"]);

    // Collapsed, one stream reads every current value; nothing reads history.
    expect(watchMetrics).toHaveBeenCalledTimes(1);
    const queries = (watchMetrics.mock.calls[0][0].queries ?? []).map(({ query }) => query);
    expect(queries.map((query) => [query?.metric, query?.groupBy])).toEqual([
      ["node.host.cpu.utilization", ["node"]],
      ["node.accelerator.utilization", ["device"]],
      ["node.accelerator.temperature", ["device"]],
      ["node.accelerator.power", ["device"]],
      ["node.filesystem.used", ["mountpoint", "fstype"]],
    ]);
    expect(queries.every((query) => query?.matchers?.node === "node-a")).toBe(true);
  });

  it("shows a reading older than a few seconds as no reading", async () => {
    renderSections(
      metricsClientWith(
        staticWatch(
          answer({ "node.host.cpu.utilization": [metricSeries({}, [gaugePoint(2, 0.4)])] }),
        ),
      ),
    );

    const resources = screen.getByRole("region", { name: "Resources" });
    await screen.findAllByText("90%");
    expect(rows(resources)).toContainEqual(["CPU", "—"]);
    expect(fills(resources)).toEqual([]);
  });

  it("expands rows to one-line history charts with their capacity, on one history stream", async () => {
    const user = setupUser();
    const watchMetrics = vi.fn(staticWatch(answer()));
    renderSections(metricsClientWith(watchMetrics));
    await screen.findAllByText("90%");

    await user.click(screen.getByRole("button", { name: "RAM" }));
    const ram = await screen.findByRole("group", { name: "RAM history" });
    expect(chartReadout(ram)).toContain("Used 24 GiB");
    expect(chartReadout(ram)).toContain("Capacity 64 GiB");
    expect(screen.queryByText("RAM: system memory")).not.toBeInTheDocument();

    const fast = screen.getByRole("group", { name: "GPU 0 · NVIDIA RTX 5090" });
    await user.click(within(fast).getByRole("button", { name: "VRAM" }));
    await user.click(within(fast).getByRole("button", { name: "Temperature" }));
    await user.click(screen.getByRole("button", { name: "/" }));

    expect(chartReadout(screen.getByRole("group", { name: "GPU 0 VRAM history" }))).toContain(
      "Capacity 32 GiB",
    );
    expect(chartReadout(screen.getByRole("group", { name: "GPU 0 temperature history" }))).toBe(
      "Temperature 61 °C",
    );
    const filesystem = chartReadout(screen.getByRole("group", { name: "/ filesystem history" }));
    expect(filesystem).toContain("Used 256 GiB");
    expect(filesystem).toContain("Capacity 1 TiB");

    // Every row's history shares one stream at the finest range.
    expect(watchMetrics).toHaveBeenCalledTimes(2);
    expect((watchMetrics.mock.calls[1][0].queries ?? []).map(({ query }) => query?.metric)).toEqual(
      [
        "node.host.memory.total",
        "node.host.memory.available",
        "node.host.cpu.utilization",
        "node.accelerator.memory.used",
        "node.accelerator.utilization",
        "node.accelerator.temperature",
        "node.accelerator.power",
        "node.filesystem.used",
        "node.filesystem.size",
      ],
    );
  });

  it("has no signal rows for a GPU without supported signals and no VRAM row on unified memory", () => {
    renderSections(
      metricsClientWith(staticWatch(answer())),
      snapshot([
        {
          deviceId: "apple-gpu-0",
          memoryArchitecture: AcceleratorMemoryArchitecture.UNIFIED,
          model: "Apple M3 Max",
          vendor: AcceleratorVendor.APPLE,
        },
        {
          deviceId: "GPU-gb10",
          memoryArchitecture: AcceleratorMemoryArchitecture.UNIFIED,
          model: "NVIDIA GB10",
          supportedSignals: [AcceleratorSignal.UTILIZATION, AcceleratorSignal.POWER],
          vendor: AcceleratorVendor.NVIDIA,
        },
      ]),
      [hostPool(64n * BigInt(gib), [other(24n * BigInt(gib))])],
    );

    expect(rows(screen.getByRole("group", { name: "GPU 0 · Apple M3 Max" }))).toEqual([]);
    expect(
      rows(screen.getByRole("group", { name: "GPU 1 · NVIDIA GB10" })).map(([label]) => label),
    ).toEqual(["Utilization", "Power"]);
  });
});

type Query = MessageInitShape<typeof MetricQuerySchema> | undefined;

/**
 * Answers over ten one-second steps whose last step is open; a gauge's open step is a real
 * reading, so utilization and CPU read 90% from it. Power peaks at 400 W earlier in the window.
 */
function answer(overrides: Record<string, ReturnType<typeof metricSeries>[]> = {}) {
  const latest = (labels: Record<string, string>, value: number, open = value) =>
    metricSeries(labels, [gaugePoint(8, value), gaugePoint(9, open, true)]);
  const byMetric: Record<string, ReturnType<typeof metricSeries>[]> = {
    "node.accelerator.memory.used": [latest({ device: "GPU-fast" }, 2 * gib)],
    "node.accelerator.power": [
      metricSeries({ device: "GPU-fast" }, [gaugePoint(1, 400), gaugePoint(8, 200)]),
    ],
    "node.accelerator.temperature": [latest({ device: "GPU-fast" }, 61)],
    "node.accelerator.utilization": [
      latest({ device: "GPU-fast" }, 0.5, 0.9),
      latest({ device: "GPU-slow" }, 0.25, 0.9),
    ],
    "node.filesystem.size": [latest({ fstype: "ext4", mountpoint: "/" }, 1024 * gib)],
    "node.filesystem.used": [latest({ fstype: "ext4", mountpoint: "/" }, 256 * gib)],
    "node.host.cpu.utilization": [latest({}, 0.375, 0.9)],
    "node.host.memory.available": [latest({}, 40 * gib)],
    "node.host.memory.total": [latest({}, 64 * gib)],
    ...overrides,
  };

  return (query: Query) =>
    queryResult({ series: byMetric[query?.metric ?? ""] ?? [], stepCount: 10 });
}

/**
 * The sections for `hardware`, drawing memory from `pools`: by default the RTX 5090's 32 GiB with
 * 2 GiB in use, the RTX 4090 of unknown size, and 64 GiB of host memory with 24 GiB in use.
 */
function renderSections(
  client: MetricsClient,
  hardware: HardwareSnapshot = snapshot(),
  pools = [
    devicePool(32n * BigInt(gib), [other(2n * BigInt(gib))], {
      deviceId: "GPU-fast",
      deviceIndex: 0,
      deviceName: "NVIDIA RTX 5090",
    }),
    devicePool(undefined, [], {
      deviceId: "GPU-slow",
      deviceIndex: 1,
      deviceName: "NVIDIA RTX 4090",
    }),
    hostPool(64n * BigInt(gib), [other(24n * BigInt(gib))]),
  ],
) {
  const memory = projectNodeMemory(
    nodeMemory("node-a", pools),
    { machineId: "node-a", name: "node-a" },
    { deployments: [], readFailed: false },
  );

  return render(
    <MachineMetricSections
      client={client}
      machineId="node-a"
      memory={memory}
      snapshot={hardware}
    />,
  );
}

/** A dedicated RTX 5090 with every signal, an RTX 4090 with utilization only, and one filesystem. */
function snapshot(
  facts: MessageInitShape<typeof AcceleratorFactsSchema>[] = [
    {
      deviceId: "GPU-fast",
      memoryArchitecture: AcceleratorMemoryArchitecture.DEDICATED,
      model: "NVIDIA RTX 5090",
      supportedSignals: [
        AcceleratorSignal.UTILIZATION,
        AcceleratorSignal.TEMPERATURE,
        AcceleratorSignal.POWER,
      ],
      totalMemoryBytes: 32n * BigInt(gib),
      usedMemoryBytes: 2n * BigInt(gib),
      vendor: AcceleratorVendor.NVIDIA,
    },
    {
      deviceId: "GPU-slow",
      model: "NVIDIA RTX 4090",
      supportedSignals: [AcceleratorSignal.UTILIZATION],
      vendor: AcceleratorVendor.NVIDIA,
    },
  ],
): HardwareSnapshot {
  return create(HardwareSnapshotSchema, {
    accelerators: [{ result: { case: "facts", value: { facts } } }],
    host: {
      result: {
        case: "facts",
        value: {
          availableMemoryBytes: 40n * BigInt(gib),
          cpuCount: 16,
          filesystems: [{ fstype: "ext4", mountpoint: "/", sizeBytes: 1024n * BigInt(gib) }],
          totalMemoryBytes: 64n * BigInt(gib),
        },
      },
    },
    observedAt: { seconds: 1n },
  });
}

/** Each metric row's label and value, in order. */
function rows(container: HTMLElement): string[][] {
  return [...container.querySelectorAll('[data-slot="metric-row"]')].map((row) => [
    row.querySelector('[data-slot="metric-row-toggle"]')?.textContent ?? "",
    row.lastElementChild?.textContent ?? "",
  ]);
}

/** The widths of the plain bars' fills, in order. */
function fills(container: HTMLElement): string[] {
  return [...container.querySelectorAll<HTMLElement>('[data-slot="metric-bar-fill"]')].map(
    (fill) => fill.style.width,
  );
}

/** The readout text above a chart's plot: each line's value, and the time only while hovered. */
function chartReadout(chart: HTMLElement): string {
  return [...(chart.firstElementChild?.querySelectorAll("span") ?? [])]
    .filter((span) => span.childElementCount === 0)
    .map((span) => span.textContent)
    .filter(Boolean)
    .join(" ");
}
