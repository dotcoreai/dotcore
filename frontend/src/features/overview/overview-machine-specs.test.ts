import { describe, expect, it } from "vitest";

import { noMachineFacts } from "@/features/machines/machine-site-values";
import { device, facts, grid, node } from "@/features/machines/machine-site-values-test-support";
import type { MetricStatsGrid } from "@/features/metrics/metric-grid";
import { MetricReduce } from "@/gen/dotcore/metrics/v1/metric_pb";

import { machineCardSpecs } from "./overview-machine-specs";

const answer = (stats: MetricStatsGrid | undefined) => ({ grid: undefined, stats });

function spec(id: string, withFacts = facts) {
  const found = machineCardSpecs(withFacts).find((entry) => entry.id === id);
  if (!found) throw new Error(`missing spec ${id}`);
  return found;
}

describe("machine card readings over the range", () => {
  it("queries each card per machine, at the bucket mean for its value and an extreme for its peak", () => {
    const reads = Object.fromEntries(
      machineCardSpecs(facts).map((card) => [
        card.id,
        card.queries.map(
          (query) =>
            `${query.metric} by ${query.groupBy.join("+")} ${MetricReduce[query.reduce ?? 0]}`,
        ),
      ]),
    );

    expect(reads).toEqual({
      cpu: ["node.host.cpu.utilization by node AVG", "node.host.cpu.utilization by node MAX"],
      gpu: ["node.accelerator.utilization by node AVG", "node.accelerator.utilization by node MAX"],
      "gpu-power": ["node.accelerator.power by node AVG", "node.accelerator.power by node MAX"],
      "gpu-temperature": [
        "node.accelerator.temperature by node AVG",
        "node.accelerator.temperature by node MAX",
      ],
      ram: [
        "node.host.memory.total by node AVG",
        "node.host.memory.available by node AVG",
        "node.host.memory.available by node MIN",
      ],
      storage: [
        "node.filesystem.used by node AVG",
        "node.filesystem.size by node AVG",
        "node.filesystem.used by node MAX",
      ],
      vram: [
        "node.accelerator.memory.used by node+device AVG",
        "node.accelerator.memory.total by node+device AVG",
        "node.accelerator.memory.used by node+device MAX",
      ],
    });
  });

  it("weights CPU by logical CPUs per step, with the peak from the per-step highs", () => {
    const means = grid(node("big", [0.5, 0.2]), node("small", [0.1, null]));
    const highs = grid(node("big", [0.9, 0.3]), node("small", [0.5, null]));

    const reading = spec("cpu").read([answer(means), answer(highs)]);

    // Step 0: (0.5 * 48 + 0.1 * 16) / 64 = 0.4; step 1: only "big" reports.
    expect(reading.sparkline[0]).toBeCloseTo(0.4);
    expect(reading.sparkline[1]).toBeCloseTo(0.2);
    expect(reading.value).toBeCloseTo(0.3);
    // Step 0 highs: (0.9 * 48 + 0.5 * 16) / 64 = 0.8.
    expect(reading.line?.label).toBe("peak");
    expect(reading.line?.value).toBeCloseTo(0.8);
  });

  it("reads a spike between samples as the GPU peak, not the step's mean", () => {
    const reading = spec("gpu").read([
      answer(grid(node("big", [0.2, 0.4]))),
      answer(grid(node("big", [0.5, 0.95]))),
    ]);

    expect(reading.value).toBeCloseTo(0.3);
    expect(reading.line?.value).toBe(0.95);
  });

  it("reads the hottest machine for temperature and sums power", () => {
    const temperature = spec("gpu-temperature").read([
      answer(grid(node("big", [61, 70, null]), node("small", [64, 66, null]))),
      answer(grid(node("big", [65, 74, null]), node("small", [68, 69, null]))),
    ]);
    const power = spec("gpu-power").read([
      answer(grid(node("big", [80, 200]), node("small", [20, 100]))),
      answer(grid(node("big", [120, 260]), node("small", [30, 140]))),
    ]);

    expect(temperature.sparkline).toEqual([64, 70, null]);
    expect(temperature.line?.value).toBe(74);
    expect(power.value).toBe(200);
    expect(power.line?.value).toBe(400);
  });

  it("sums each machine's used storage, its peak from the per-step highs", () => {
    const reading = spec("storage").read([
      answer(grid(node("big", [200, 300]), node("small", [50, 60]))),
      answer(grid(node("big", [1000, 1000]), node("small", [200, 200]))),
      answer(grid(node("big", [250, 400]), node("small", [60, 70]))),
    ]);

    expect(reading.sparkline).toEqual([250, 360]);
    // Step 1: 400 + 70 = 470.
    expect(reading.line?.value).toBe(470);
  });

  it("counts dedicated GPU memory only for VRAM", () => {
    const used = grid(
      device("big", "gpu0", [10, 20]),
      device("big", "gpu-unified", [500, 500]),
      device("small", "gpu0", [7, 7]),
    );
    const total = grid(
      device("big", "gpu0", [24, 24]),
      device("big", "gpu-unified", [900, 900]),
      device("small", "gpu0", [16, 16]),
    );
    const usedHighs = grid(
      device("big", "gpu0", [12, 22]),
      device("big", "gpu-unified", [600, 600]),
      device("small", "gpu0", [9, 9]),
    );

    const dedicated = spec("vram").read([answer(used), answer(total), answer(usedHighs)]);
    const unified = spec("vram", noMachineFacts).read([
      answer(used),
      answer(total),
      answer(usedHighs),
    ]);

    expect(dedicated.sparkline).toEqual([10, 20]);
    expect(dedicated.line?.value).toBe(22);
    expect(unified.value).toBeNull();
  });

  it("reads used RAM as total minus available, its peak from the lowest available", () => {
    const reading = spec("ram").read([
      answer(grid(node("big", [64, 64]), node("small", [32, 32]))),
      answer(grid(node("big", [40, 24]), node("small", [30, 30]))),
      answer(grid(node("big", [30, 20]), node("small", [28, 30]))),
    ]);

    expect(reading.sparkline).toEqual([26, 42]);
    // Step 1: (64 - 20) + (32 - 30) = 46.
    expect(reading.line?.value).toBe(46);
  });

  it("charts RAM by machine from its own answers as each machine's used memory", () => {
    const ram = spec("ram");
    const chart = ram.splitChart?.(
      [
        answer(grid(node("big", [64, 64]), node("small", [32, 32]))),
        answer(grid(node("big", [40, 24]), node("small", [30, 30]))),
        answer(grid(node("big", [30, 20]), node("small", [28, 30]))),
      ],
      { groupBy: ["node"], id: "machine", label: "By machine" },
    );

    expect(chart?.map((series) => [series.id, series.values])).toEqual([
      ["big", [24, 40]],
      ["small", [2, 2]],
    ]);
  });
});
