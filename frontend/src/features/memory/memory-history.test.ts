import { create } from "@bufbuild/protobuf";
import { renderHook } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { fillMetricGrid, type MetricGrid } from "@/features/metrics/metric-grid";
import { gaugePoint, metricSeries, queryResult } from "@/features/metrics/metric-test-support";
import { ModelDeploymentSchema } from "@/gen/dotcore/model/v1/deployment_pb";
import {
  AcceleratorObservationSchema,
  AcceleratorVendor,
} from "@/gen/dotcore/observation/v1/observation_pb";

import type { DescribedMachine } from "./described-machines";
import { gb10Snapshot, hardwareSnapshot, workstationSnapshot } from "./machine-test-support";
import {
  memoryQueryGroups,
  type ModelLineRef,
  modelMemoryLines,
  ramHistory,
  vramHistory,
} from "./memory-history";
import { useMemoryHistory } from "./use-memory-history";

const gib = 1024 ** 3;
const names = new Map([
  ["gpu1", "gpu1"],
  ["spark", "spark-01"],
  ["mbp", "mbp"],
]);
const models = new Map<string, ModelLineRef>([
  ["gemma", { colorKey: "model-gemma", name: "Gemma 3 27B" }],
  ["llama", { colorKey: "model-llama", name: "Llama 3.1 8B" }],
  ["qwen", { colorKey: "model-qwen", name: "Qwen 3 32B" }],
]);

describe("memoryQueryGroups", () => {
  it("reads deployment memory only in rows a described machine runs models in", () => {
    const deploymentQuery = {
      groupBy: ["deployment", "node", "device"],
      matchers: {},
      metric: "deployment.accelerator.memory.used",
    };

    expect(memoryQueryGroups([])).toEqual([]);
    expect(memoryQueryGroups([gpu1()])).toEqual([
      {
        current: [],
        history: [
          { groupBy: ["node"], matchers: {}, metric: "node.host.memory.total" },
          { groupBy: ["node"], matchers: {}, metric: "node.host.memory.available" },
        ],
        id: "ram",
      },
      {
        current: [],
        history: [
          { groupBy: ["node", "device"], matchers: {}, metric: "node.accelerator.memory.used" },
          { groupBy: ["node", "device"], matchers: {}, metric: "node.accelerator.memory.total" },
          deploymentQuery,
        ],
        id: "vram",
      },
    ]);
    expect(memoryQueryGroups([spark()]).map((group) => group.history.at(-1))).toEqual([
      deploymentQuery,
    ]);
    // Unreadable GPUs leave the machine's model row unknown, so nothing is read for it.
    expect(memoryQueryGroups([unreadable()])[0].history).toHaveLength(2);
  });
});

describe("ramHistory and vramHistory", () => {
  it("pairs used memory and capacity over the same machines at each step", () => {
    const total = grid([nodeGauge("gpu1", [62, 62, 62]), nodeGauge("spark", [119, null, 119])], 3);
    const available = grid(
      [nodeGauge("gpu1", [47, 47, 40]), nodeGauge("spark", [31, 31, null])],
      3,
    );

    const [used, capacity] = ramHistory(total, available, names, "combined");

    expect(used.values).toEqual([15 + 88, 15, 22]);
    expect(capacity).toMatchObject({ dashed: true, id: "capacity" });
    expect(capacity.values).toEqual([62 + 119, 62, 62]);
  });

  it("keeps a machine not counted now in history until it stopped reporting", () => {
    // mbp is disconnected now; its line still covers the steps it reported.
    const total = grid([nodeGauge("gpu1", [62, 62]), nodeGauge("mbp", [64, null])], 2);
    const available = grid([nodeGauge("gpu1", [47, 47]), nodeGauge("mbp", [23, null])], 2);

    expect(ramHistory(total, available, names, "machines")).toMatchObject([
      { color: { resource: "gpu1" }, values: [15, 15] },
      { color: { resource: "mbp" }, label: "mbp", values: [41, null] },
    ]);
    expect(ramHistory(total, available, names, "combined")[0].values).toEqual([56, 15]);
  });

  it("draws only dedicated GPU memory the described snapshots know of", () => {
    const used = grid(
      [deviceGauge("gpu1", "gpu-0", [0.5 * gib]), deviceGauge("spark", "gb10", [60 * gib])],
      1,
    );
    const total = grid(
      [deviceGauge("gpu1", "gpu-0", [32 * gib]), deviceGauge("spark", "gb10", [119 * gib])],
      1,
    );

    const [usedLine, capacity] = vramHistory(used, total, [gpu1(), spark()], names, "combined");

    expect(usedLine.values).toEqual([0.5 * gib]);
    expect(capacity.values).toEqual([32 * gib]);
  });
});

describe("modelMemoryLines", () => {
  const described = [gpu1(), spark(), unreadable(), noGpu()];
  const deploymentGrid = grid(
    [
      deploymentGauge("gemma", "gpu1", "gpu-0", [20, 21]),
      deploymentGauge("llama", "spark", "gb10", [8, 9]),
      deploymentGauge("qwen", "lab", "gpu-0", [30, 30]),
    ],
    2,
  );

  it("draws a deployment in the row its machine runs models in", () => {
    // gpu1 has a dedicated GPU, spark a unified one; lab's GPUs are unreadable, and nogpu runs
    // models in RAM without a measurement, so neither draws.
    expect(modelMemoryLines(deploymentGrid, deploymentGrid, described, models, "vram")).toEqual([
      {
        color: { resource: "model-gemma" },
        id: "model:gemma",
        label: "Gemma 3 27B",
        provisional: [false, false],
        values: [20, 21],
      },
    ]);
    expect(modelMemoryLines(deploymentGrid, deploymentGrid, described, models, "ram")).toEqual([
      {
        color: { resource: "model-llama" },
        id: "model:llama",
        label: "Llama 3.1 8B",
        provisional: [false, false],
        values: [8, 9],
      },
    ]);
  });

  it("sums one deployment's devices per step and keeps gaps absent", () => {
    const rig: DescribedMachine = {
      machineId: "rig",
      name: "rig",
      snapshot: hardwareSnapshot({
        cpuCount: 64,
        devices: [
          { deviceId: "gpu-0", vram: { totalGib: 24, usedGib: 1 } },
          { deviceId: "gpu-1", vram: { totalGib: 24, usedGib: 1 } },
        ],
      }),
    };
    const split = grid(
      [
        deploymentGauge("gemma", "rig", "gpu-0", [1, 2, null]),
        deploymentGauge("gemma", "rig", "gpu-1", [3, null, null]),
      ],
      3,
    );

    expect(modelMemoryLines(split, split, [rig], models, "vram")[0].values).toEqual([4, 2, null]);
  });

  it("names a deployment the page never saw as removed, colored by its id", () => {
    const removed = grid([deploymentGauge("gone", "gpu1", "gpu-0", [5])], 1);

    expect(modelMemoryLines(removed, removed, [gpu1()], models, "vram")).toMatchObject([
      { color: { resource: "gone" }, label: "Removed model" },
    ]);
  });

  it("orders lines by their latest value, largest first, then by deployment", () => {
    const several = grid(
      [
        deploymentGauge("llama", "gpu1", "gpu-0", [9, 3]),
        deploymentGauge("gemma", "gpu1", "gpu-0", [1, 5]),
        deploymentGauge("qwen", "gpu1", "gpu-0", [5, null]),
      ],
      2,
    );

    expect(
      modelMemoryLines(several, several, [gpu1()], models, "vram").map((line) => line.id),
    ).toEqual(["model:gemma", "model:qwen", "model:llama"]);
  });

  it("pairs steps by time with the chart's base grid", () => {
    const base = grid([nodeGauge("gpu1", [1, 1, 1])], 3);
    // The deployment grid covers only the last two of the base grid's steps.
    const later = fillMetricGrid(
      queryResult({
        series: [
          metricSeries({ deployment: "gemma", device: "gpu-0", node: "gpu1" }, [
            gaugePoint(1, 7),
            gaugePoint(2, 8),
          ]),
        ],
        startOffset: 1,
        stepCount: 2,
      }),
    );

    expect(modelMemoryLines(base, later, [gpu1()], models, "vram")[0].values).toEqual([null, 7, 8]);
  });
});

describe("useMemoryHistory", () => {
  it("keeps the name of a deployment removed while the page was open", () => {
    const gemma = create(ModelDeploymentSchema, {
      modelDeploymentId: "gemma",
      modelDeploymentName: "Gemma 3 27B",
      snapshot: { source: { case: "local", value: { modelId: "gemma-3-27b" } } },
    });
    const { rerender, result } = renderHook(
      ({ deployments }) => useMemoryHistory([], new Map(), deployments),
      { initialProps: { deployments: [gemma] } },
    );

    rerender({ deployments: [] });

    expect(result.current.models.get("gemma")).toEqual({
      colorKey: "gemma",
      name: "Gemma 3 27B",
    });
  });
});

function gpu1(): DescribedMachine {
  return { machineId: "gpu1", name: "gpu1", snapshot: workstationSnapshot() };
}

function spark(): DescribedMachine {
  return { machineId: "spark", name: "spark-01", snapshot: gb10Snapshot() };
}

/** A machine whose GPU vendor observation failed, so its model row is unknown. */
function unreadable(): DescribedMachine {
  const snapshot = hardwareSnapshot({ cpuCount: 8, memory: { availableGib: 8, totalGib: 32 } });
  snapshot.accelerators = [
    create(AcceleratorObservationSchema, {
      result: { case: "unavailable", value: { code: "NVML_FAILED", message: "failed" } },
      vendor: AcceleratorVendor.NVIDIA,
    }),
  ];
  return { machineId: "lab", name: "lab", snapshot };
}

/** A machine without GPUs, such as one running models in Docker on its CPU. */
function noGpu(): DescribedMachine {
  return {
    machineId: "nogpu",
    name: "nogpu",
    snapshot: hardwareSnapshot({ cpuCount: 8, memory: { availableGib: 8, totalGib: 32 } }),
  };
}

/** A one-second grid of `stepCount` steps; the last step is the newest. */
function grid(series: ReturnType<typeof metricSeries>[], stepCount = 1): MetricGrid {
  return fillMetricGrid(queryResult({ series, stepCount }));
}

function nodeGauge(node: string, values: (number | null)[]) {
  return metricSeries({ node }, gaugePoints(values));
}

function deviceGauge(node: string, device: string, values: (number | null)[]) {
  return metricSeries({ device, node }, gaugePoints(values));
}

function deploymentGauge(
  deployment: string,
  node: string,
  device: string,
  values: (number | null)[],
) {
  return metricSeries({ deployment, device, node }, gaugePoints(values));
}

function gaugePoints(values: (number | null)[]) {
  return values.flatMap((value, index) => (value === null ? [] : [gaugePoint(index, value)]));
}
