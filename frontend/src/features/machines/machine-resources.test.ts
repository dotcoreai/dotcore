import { describe, expect, it } from "vitest";

import type { DescribedMachine } from "@/features/memory/described-machines";
import {
  gb10Snapshot,
  hardwareSnapshot,
  listedMachine,
  macSnapshot,
  workstationSnapshot,
} from "@/features/memory/machine-test-support";
import type { MachineSnapshotDetail } from "@/features/memory/use-hardware-snapshots";
import { fillMetricGrid, type MetricGrid } from "@/features/metrics/metric-grid";
import { gaugePoint, metricSeries, queryResult } from "@/features/metrics/metric-test-support";
import { AcceleratorSignal } from "@/gen/dotcore/observation/v1/observation_pb";
import { NodeObservationState } from "@/gen/dotcore/site/v1/service_pb";

import {
  countMachines,
  cpuHistory,
  cpuReading,
  gpuReading,
  resourceQueryGroups,
  storageReading,
} from "./machine-resources";

const gib = 1024 ** 3;
const names = new Map([
  ["gpu1", "gpu1"],
  ["spark", "spark-01"],
  ["mbp", "mbp"],
]);

describe("countMachines", () => {
  it("counts connected machines with a snapshot and names the others with their state", () => {
    const machines = [
      machine("gpu1", NodeObservationState.CONNECTED),
      machine("spark", NodeObservationState.STALE),
      machine("mbp", NodeObservationState.DISCONNECTED),
      machine("lab", NodeObservationState.MISSING),
      machine("broken", NodeObservationState.CONNECTED),
      machine("booting", NodeObservationState.CONNECTED),
    ];
    const details = new Map<string, MachineSnapshotDetail>([
      ["gpu1", { kind: "success", snapshot: gpu1().snapshot }],
      ["broken", { kind: "error", message: "unreadable" }],
      ["booting", { kind: "loading" }],
    ]);

    const { counted, uncounted } = countMachines(machines, details);

    expect(counted.map((entry) => entry.machineId)).toEqual(["gpu1"]);
    expect(uncounted.map(({ name, state }) => [name, state])).toEqual([
      ["spark", "stale"],
      ["mbp", "disconnected"],
      ["lab", "missing"],
      ["broken", "hardware unavailable"],
    ]);
  });
});

describe("cpuReading", () => {
  it("averages CPU utilization weighted by logical CPUs", () => {
    const reading = cpuReading(
      [gpu1(), spark()],
      grid([nodeGauge("gpu1", [0.12]), nodeGauge("spark", [0.46])]),
    );

    // (0.12 × 32 + 0.46 × 20) / 52
    expect(reading.value).toBeCloseTo(0.2508, 4);
    expect(reading.capacity).toBe(52);
    expect(reading.leftOut).toEqual([]);
    expect(reading.parts.map(({ capacity, name, value }) => [name, capacity, value])).toEqual([
      ["gpu1", 32, 0.12],
      ["spark-01", 20, 0.46],
    ]);
  });

  it("leaves a machine without a current reading out of the value and the CPU count", () => {
    const reading = cpuReading(
      [gpu1(), spark()],
      // spark's last reading is 10 steps old, beyond the freshness bound.
      grid([nodeGauge("gpu1", [0.12], 10), nodeGauge("spark", [0.46])], 11),
    );

    expect(reading.value).toBeCloseTo(0.12, 6);
    expect(reading.capacity).toBe(32);
    expect(reading.leftOut).toEqual([{ name: "spark-01", reason: "noReading" }]);
  });
});

describe("gpuReading", () => {
  it("averages reporting GPUs, sums their power, and tells unsupported from unread GPUs", () => {
    const reading = gpuReading(
      [gpu1(), spark(), mbp(), lab()],
      grid([deviceGauge("gpu1", "gpu-0", [0.04]), deviceGauge("spark", "gb10", [0.71])]),
      grid([deviceGauge("gpu1", "gpu-0", [38]), deviceGauge("spark", "gb10", [92])]),
    );

    expect(reading?.value).toBeCloseTo(0.375, 6);
    expect(reading?.capacity).toBe(2);
    expect(reading?.powerWatts).toBe(130);
    expect(reading?.leftOut).toEqual([
      { name: "mbp", reason: "unsupported" },
      { name: "lab", reason: "noReading" },
    ]);
    expect(reading?.parts.map((part) => part.name)).toEqual(["gpu1", "spark-01", "lab"]);
  });

  it("names a machine whose GPUs only partly report, with its share", () => {
    const twoGpus: DescribedMachine = {
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

    const reading = gpuReading([twoGpus], grid([deviceGauge("rig", "gpu-0", [0.8])]), undefined);

    expect(reading?.value).toBeCloseTo(0.8, 6);
    expect(reading?.capacity).toBe(1);
    expect(reading?.leftOut).toEqual([{ name: "rig (1 of 2 GPUs)", reason: "noReading" }]);
  });

  it("has no reading when no GPU supports utilization", () => {
    expect(gpuReading([mbp()], undefined, undefined)).toBeUndefined();
  });
});

describe("storageReading", () => {
  it("sums used storage against the summed sizes of machines with a reading", () => {
    const reading = storageReading(
      [gpu1(), spark()],
      grid([nodeGauge("gpu1", [900 * gib], 10), nodeGauge("spark", [1000 * gib])], 11),
    );

    expect(reading?.value).toBe(900 * gib);
    expect(reading?.capacity).toBe((1863 + 3726) * gib);
    expect(reading?.leftOut).toEqual([{ name: "spark-01", reason: "noReading" }]);
  });
});

describe("resourceQueryGroups", () => {
  it("reads nothing without a counted machine, and only groups the counted machines can fill", () => {
    expect(resourceQueryGroups([])).toEqual([]);
    expect(resourceQueryGroups([mbp()]).map((group) => group.id)).toEqual([
      "cpu",
      "ram",
      "storage",
    ]);
    expect(resourceQueryGroups([gpu1(), mbp()]).map((group) => group.id)).toEqual([
      "cpu",
      "ram",
      "vram",
      "storage",
      "gpu",
    ]);
    expect(resourceQueryGroups([gpu1()])[0].current[0]).toEqual({
      groupBy: ["node"],
      matchers: {},
      metric: "node.host.cpu.utilization",
    });
  });
});

describe("history", () => {
  it("combines CPU per step over the machines reporting in it, weighted by CPUs", () => {
    const utilization = grid(
      [nodeGauge("gpu1", [0.1, 0.2, 0.3]), nodeGauge("spark", [0.5, null, 0.5])],
      3,
    );

    const [combined] = cpuHistory(utilization, [gpu1(), spark()], names, "combined");

    expect(combined.values[0]).toBeCloseTo((0.1 * 32 + 0.5 * 20) / 52, 6);
    // spark reported nothing at step 1, so that step is gpu1's alone.
    expect(combined.values[1]).toBeCloseTo(0.2, 6);
    expect(cpuHistory(utilization, [gpu1(), spark()], names, "machines")).toMatchObject([
      { color: { resource: "gpu1" }, label: "gpu1", values: [0.1, 0.2, 0.3] },
      { color: { resource: "spark" }, label: "spark-01", values: [0.5, null, 0.5] },
    ]);
  });

  it("keeps a machine described by its last snapshot in the combined CPU line", () => {
    // spark is not counted now, but its last snapshot still weighs the steps it reported.
    const utilization = grid([nodeGauge("gpu1", [0.1, 0.1]), nodeGauge("spark", [0.5, null])], 2);

    const [combined] = cpuHistory(utilization, [gpu1(), spark()], names, "combined");

    expect(combined.values[0]).toBeCloseTo((0.1 * 32 + 0.5 * 20) / 52, 6);
    expect(combined.values[1]).toBeCloseTo(0.1, 6);
    // A machine never described has no CPU count to weigh it.
    expect(cpuHistory(utilization, [gpu1()], names, "combined")[0].values[0]).toBeCloseTo(0.1, 6);
  });
});

function machine(machineId: string, state: NodeObservationState) {
  return listedMachine(machineId, machineId, state);
}

function gpu1(): DescribedMachine {
  return { machineId: "gpu1", name: "gpu1", snapshot: workstationSnapshot() };
}

function spark(): DescribedMachine {
  return { machineId: "spark", name: "spark-01", snapshot: gb10Snapshot() };
}

function mbp(): DescribedMachine {
  return { machineId: "mbp", name: "mbp", snapshot: macSnapshot() };
}

/** A machine whose GPU supports utilization but sent no reading. */
function lab(): DescribedMachine {
  return {
    machineId: "lab",
    name: "lab",
    snapshot: hardwareSnapshot({
      cpuCount: 8,
      devices: [
        {
          deviceId: "gpu-0",
          supportedSignals: [AcceleratorSignal.UTILIZATION],
          vram: { totalGib: 24, usedGib: 0 },
        },
      ],
    }),
  };
}

/** A one-second grid of `stepCount` steps; the last step is the newest. */
function grid(series: ReturnType<typeof metricSeries>[], stepCount = 1): MetricGrid {
  return fillMetricGrid(queryResult({ series, stepCount }));
}

/** A node's gauge with one value per step from `offset`; `null` leaves the step absent. */
function nodeGauge(node: string, values: (number | null)[], offset = 0) {
  return metricSeries({ node }, gaugePoints(values, offset));
}

function deviceGauge(node: string, device: string, values: (number | null)[]) {
  return metricSeries({ device, node }, gaugePoints(values, 0));
}

function gaugePoints(values: (number | null)[], offset: number) {
  return values.flatMap((value, index) =>
    value === null ? [] : [gaugePoint(offset + index, value)],
  );
}
