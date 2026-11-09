import { describe, expect, it } from "vitest";

import { fillMetricGrid, type MetricGrid } from "@/features/metrics/metric-grid";
import { gaugePoint, metricSeries, queryResult } from "@/features/metrics/metric-test-support";

import type { DescribedMachine } from "./described-machines";
import { gb10Snapshot, workstationSnapshot } from "./machine-test-support";
import { memoryQueryGroups, ramHistory, vramHistory } from "./memory-history";

const gib = 1024 ** 3;
const names = new Map([
  ["gpu1", "gpu1"],
  ["spark", "spark-01"],
  ["mbp", "mbp"],
]);
describe("memoryQueryGroups", () => {
  it("reads RAM history, and VRAM history only with a described dedicated GPU", () => {
    const ram = {
      current: [],
      history: [
        { groupBy: ["node"], matchers: {}, metric: "node.host.memory.total" },
        { groupBy: ["node"], matchers: {}, metric: "node.host.memory.available" },
      ],
      id: "ram",
    };

    expect(memoryQueryGroups([])).toEqual([]);
    expect(memoryQueryGroups([gpu1()])).toEqual([
      ram,
      {
        current: [],
        history: [
          { groupBy: ["node", "device"], matchers: {}, metric: "node.accelerator.memory.used" },
          { groupBy: ["node", "device"], matchers: {}, metric: "node.accelerator.memory.total" },
        ],
        id: "vram",
      },
    ]);
    // A unified GPU's memory is the host's.
    expect(memoryQueryGroups([spark()])).toEqual([ram]);
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

function gpu1(): DescribedMachine {
  return { machineId: "gpu1", name: "gpu1", snapshot: workstationSnapshot() };
}

function spark(): DescribedMachine {
  return { machineId: "spark", name: "spark-01", snapshot: gb10Snapshot() };
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

function gaugePoints(values: (number | null)[]) {
  return values.flatMap((value, index) => (value === null ? [] : [gaugePoint(index, value)]));
}
